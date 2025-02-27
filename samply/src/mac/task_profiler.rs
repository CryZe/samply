use crossbeam_channel::Receiver;
use framehop::{
    CacheNative, FrameAddress, MayAllocateDuringUnwind, Module, ModuleUnwindData, TextByteData,
    Unwinder, UnwinderNative,
};
use fxprof_processed_profile::debugid::DebugId;
use fxprof_processed_profile::{LibraryInfo, ProcessHandle, Profile, ThreadHandle, Timestamp};
use mach::mach_types::thread_act_port_array_t;
use mach::mach_types::thread_act_t;
use mach::message::mach_msg_type_number_t;
use mach::port::mach_port_t;
use mach::task::task_threads;
use mach::traps::mach_task_self;
use mach::vm::mach_vm_deallocate;
use mach::vm_types::{mach_vm_address_t, mach_vm_size_t};
use object::{CompressedFileRange, CompressionFormat, Object, ObjectSection};
use samply_symbols::{object, DebugIdExt};
use wholesym::samply_symbols;

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::mem;
use std::ops::Deref;
use std::path::{Path, PathBuf};

use crate::shared::jit_category_manager::JitCategoryManager;
use crate::shared::jit_function_recycler::JitFunctionRecycler;
use crate::shared::jitdump_manager::JitDumpManager;
use crate::shared::lib_mappings::{
    LibMappingAdd, LibMappingInfo, LibMappingOp, LibMappingOpQueue, LibMappingRemove,
};
use crate::shared::perf_map::try_load_perf_map;
use crate::shared::process_sample_data::ProcessSampleData;
use crate::shared::recycling::{ProcessRecycler, ProcessRecyclingData, ThreadRecycler};
use crate::shared::timestamp_converter::TimestampConverter;
use crate::shared::unresolved_samples::{UnresolvedSamples, UnresolvedStacks};

use super::error::SamplingError;
use super::kernel_error::{IntoResult, KernelError};
use super::proc_maps::{DyldInfo, DyldInfoManager, Modification, StackwalkerRef, VmSubData};
use super::sampler::TaskInit;
use super::thread_profiler::{get_thread_id, get_thread_name, ThreadProfiler};

pub enum UnwindSectionBytes {
    Remapped(VmSubData),
    Mmap(MmapSubData),
    Allocated(Vec<u8>),
}

impl Deref for UnwindSectionBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            UnwindSectionBytes::Remapped(vm_sub_data) => vm_sub_data.deref(),
            UnwindSectionBytes::Mmap(mmap_sub_data) => mmap_sub_data.deref(),
            UnwindSectionBytes::Allocated(vec) => vec.deref(),
        }
    }
}

pub struct MmapSubData {
    mmap: memmap2::Mmap,
    offset: usize,
    len: usize,
}

impl MmapSubData {
    pub fn try_new(mmap: memmap2::Mmap, offset: usize, len: usize) -> Option<Self> {
        let end_addr = offset.checked_add(len)?;
        if end_addr <= mmap.len() {
            Some(Self { mmap, offset, len })
        } else {
            None
        }
    }
}

impl Deref for MmapSubData {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.mmap.deref()[self.offset..][..self.len]
    }
}

pub type UnwinderCache = CacheNative<UnwindSectionBytes, MayAllocateDuringUnwind>;

pub struct TaskProfiler {
    task: mach_port_t,
    pid: u32,
    live_threads: HashMap<thread_act_t, ThreadProfiler>,
    lib_info_manager: DyldInfoManager,
    executable_name: String,
    profile_process: ProcessHandle,
    main_thread_handle: ThreadHandle,
    ignored_errors: Vec<SamplingError>,
    unwinder: UnwinderNative<UnwindSectionBytes, MayAllocateDuringUnwind>,
    jitdump_path_receiver: Receiver<PathBuf>,
    jitdump_manager: JitDumpManager,
    unresolved_samples: UnresolvedSamples,
    lib_mapping_ops: LibMappingOpQueue,
    thread_recycler: Option<ThreadRecycler>,
    jit_function_recycler: Option<JitFunctionRecycler>,
    timestamp_converter: TimestampConverter,
}

impl TaskProfiler {
    pub fn new(
        task_init: TaskInit,
        timestamp_converter: TimestampConverter,
        command_name: &str,
        profile: &mut Profile,
        mut process_recycler: Option<&mut ProcessRecycler>,
    ) -> Result<Self, SamplingError> {
        let TaskInit {
            start_time_mono,
            task,
            pid,
            jitdump_path_receiver,
        } = task_init;
        let start_time = timestamp_converter.convert_time(start_time_mono);

        let mut lib_info_manager = DyldInfoManager::new(task);
        let initial_lib_mods = lib_info_manager
            .check_for_changes()
            .map_err(|e| SamplingError::Ignorable("Could not check process libraries", e))?;
        let executable_name = initial_lib_mods
            .iter()
            .find_map(|change| match change {
                Modification::Added(lib) if lib.is_executable => Path::new(&lib.file)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string()),
                _ => None,
            })
            .unwrap_or_else(|| command_name.to_string());

        let thread_acts = get_thread_list(task)?;
        if thread_acts.is_empty() {
            return Err(SamplingError::Ignorable(
                "No threads",
                KernelError::Terminated,
            ));
        }

        let recycling_data = process_recycler
            .as_mut()
            .and_then(|r| r.recycle_by_name(&executable_name));

        let mut live_threads = HashMap::new();
        let mut thread_act_iter = thread_acts.into_iter();
        let main_thread_act = thread_act_iter.next().ok_or(SamplingError::Ignorable(
            "No main thread",
            KernelError::Terminated,
        ))?;
        let (main_thread_tid, _is_libdispatch_thread) = get_thread_id(main_thread_act)
            .map_err(|e| SamplingError::Ignorable("Could not get main thread tid", e))?;
        let main_thread_name = get_thread_name(main_thread_act)?;

        let (profile_process, main_thread_handle, mut thread_recycler, jit_function_recycler) =
            match recycling_data {
                Some(ProcessRecyclingData {
                    process_handle,
                    main_thread_handle,
                    thread_recycler,
                    jit_function_recycler,
                }) => (
                    process_handle,
                    main_thread_handle,
                    Some(thread_recycler),
                    Some(jit_function_recycler),
                ),
                None => {
                    let profile_process = profile.add_process(&executable_name, pid, start_time);
                    let main_thread_handle =
                        profile.add_thread(profile_process, main_thread_tid, start_time, true);
                    if let Some(main_thread_name) = &main_thread_name {
                        profile.set_thread_name(main_thread_handle, main_thread_name);
                    }
                    let (thread_recycler, jit_function_recycler) = match process_recycler {
                        Some(_) => (
                            Some(ThreadRecycler::new()),
                            Some(JitFunctionRecycler::default()),
                        ),
                        None => (None, None),
                    };
                    (
                        profile_process,
                        main_thread_handle,
                        thread_recycler,
                        jit_function_recycler,
                    )
                }
            };

        let main_thread = ThreadProfiler::new(
            task,
            main_thread_tid,
            main_thread_handle,
            main_thread_act,
            main_thread_name,
        );
        live_threads.insert(main_thread_act, main_thread);

        for thread_act in thread_act_iter {
            if let (Ok((tid, _is_libdispatch_thread)), Ok(name)) =
                (get_thread_id(thread_act), get_thread_name(thread_act))
            {
                let profile_thread = if let (Some(name), Some(thread_recycler)) =
                    (&name, thread_recycler.as_mut())
                {
                    if let Some(profile_thread) = thread_recycler.recycle_by_name(name) {
                        profile_thread
                    } else {
                        let profile_thread =
                            profile.add_thread(profile_process, tid, start_time, false);
                        profile.set_thread_name(profile_thread, name);
                        profile_thread
                    }
                } else {
                    let profile_thread =
                        profile.add_thread(profile_process, tid, start_time, false);
                    if let Some(name) = &name {
                        profile.set_thread_name(profile_thread, name);
                    }
                    profile_thread
                };

                let thread = ThreadProfiler::new(task, tid, profile_thread, thread_act, name);
                live_threads.insert(thread_act, thread);
            }
        }

        let mut task_profiler = TaskProfiler {
            task,
            pid,
            live_threads,
            lib_info_manager,
            executable_name,
            profile_process,
            main_thread_handle,
            ignored_errors: Vec::new(),
            unwinder: UnwinderNative::new(),
            jitdump_path_receiver,
            jitdump_manager: JitDumpManager::new_for_process(main_thread_handle),
            lib_mapping_ops: Default::default(),
            unresolved_samples: Default::default(),
            thread_recycler,
            jit_function_recycler,
            timestamp_converter,
        };

        task_profiler.process_lib_modifications(start_time_mono, initial_lib_mods, profile);

        Ok(task_profiler)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn sample(
        &mut self,
        now: Timestamp,
        now_mono: u64,
        unwinder_cache: &mut UnwinderCache,
        profile: &mut Profile,
        stack_scratch_buffer: &mut Vec<FrameAddress>,
        unresolved_stacks: &mut UnresolvedStacks,
        fold_recursive_prefix: bool,
    ) -> Result<bool, SamplingError> {
        let result = self.sample_impl(
            now,
            now_mono,
            unwinder_cache,
            profile,
            stack_scratch_buffer,
            unresolved_stacks,
            fold_recursive_prefix,
        );
        match result {
            Ok(()) => Ok(true),
            Err(SamplingError::ProcessTerminated(_, _)) => Ok(false),
            Err(err @ SamplingError::Ignorable(_, _)) => {
                self.ignored_errors.push(err);
                if self.ignored_errors.len() >= 10 {
                    println!(
                        "Treating process \"{}\" [pid: {}] as terminated after 10 unknown errors:",
                        self.executable_name, self.pid
                    );
                    println!("{:#?}", self.ignored_errors);
                    Ok(false)
                } else {
                    // Pretend that sampling worked and that the thread is still alive.
                    Ok(true)
                }
            }
            Err(err) => Err(err),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn sample_impl(
        &mut self,
        now: Timestamp,
        now_mono: u64,
        unwinder_cache: &mut UnwinderCache,
        profile: &mut Profile,
        stack_scratch_buffer: &mut Vec<FrameAddress>,
        unresolved_stacks: &mut UnresolvedStacks,
        fold_recursive_prefix: bool,
    ) -> Result<(), SamplingError> {
        // First, check for any newly-loaded libraries.
        if let Ok(changes) = self.lib_info_manager.check_for_changes() {
            self.process_lib_modifications(now_mono, changes, profile);
        }

        // Enumerate threads.
        let thread_acts = get_thread_list(self.task)?;
        let previously_live_threads: HashSet<_> = self.live_threads.keys().cloned().collect();
        let mut now_live_threads = HashSet::new();
        for thread_act in thread_acts {
            let mut entry = self.live_threads.entry(thread_act);
            let thread = match entry {
                Entry::Occupied(ref mut entry) => entry.get_mut(),
                Entry::Vacant(entry) => {
                    if let (Ok((tid, _is_libdispatch_thread)), Ok(name)) =
                        (get_thread_id(thread_act), get_thread_name(thread_act))
                    {
                        let profile_thread = if let (Some(name), Some(thread_recycler)) =
                            (&name, self.thread_recycler.as_mut())
                        {
                            if let Some(profile_thread) = thread_recycler.recycle_by_name(name) {
                                profile_thread
                            } else {
                                let profile_thread =
                                    profile.add_thread(self.profile_process, tid, now, false);
                                profile.set_thread_name(profile_thread, name);
                                profile_thread
                            }
                        } else {
                            let profile_thread =
                                profile.add_thread(self.profile_process, tid, now, false);
                            if let Some(name) = &name {
                                profile.set_thread_name(profile_thread, name);
                            }
                            profile_thread
                        };
                        let thread =
                            ThreadProfiler::new(self.task, tid, profile_thread, thread_act, name);
                        entry.insert(thread)
                    } else {
                        continue;
                    }
                }
            };
            // Grab a sample from the thread.
            let stackwalker = StackwalkerRef::new(&self.unwinder, unwinder_cache);
            thread.check_thread_name(profile, self.thread_recycler.as_mut());
            let still_alive = thread.sample(
                stackwalker,
                now,
                now_mono,
                stack_scratch_buffer,
                unresolved_stacks,
                &mut self.unresolved_samples,
                fold_recursive_prefix,
            )?;
            if still_alive {
                now_live_threads.insert(thread_act);
            }
        }
        let dead_threads = previously_live_threads.difference(&now_live_threads);
        for thread_act in dead_threads {
            let mut thread = self.live_threads.remove(thread_act).unwrap();
            thread.notify_dead(now, profile);
            let (thread_name, thread_handle) = thread.finish();
            if let (Some(thread_name), Some(thread_recycler)) =
                (thread_name, self.thread_recycler.as_mut())
            {
                thread_recycler.add_to_pool(&thread_name, thread_handle);
            }
        }
        Ok(())
    }

    fn process_lib_modifications(
        &mut self,
        now_mono: u64,
        changes: Vec<Modification<DyldInfo>>,
        profile: &mut Profile,
    ) {
        for change in changes {
            match change {
                Modification::Added(mut lib) => {
                    self.add_lib_to_unwinder_and_ensure_debug_id(&mut lib);

                    let path = Path::new(&lib.file);
                    if let Some(name) = path.file_name() {
                        let name = name.to_string_lossy();
                        let path = path.to_string_lossy();
                        let lib_handle = profile.add_lib(LibraryInfo {
                            name: name.to_string(),
                            debug_name: name.to_string(),
                            path: path.to_string(),
                            debug_path: path.to_string(),
                            debug_id: lib.debug_id.unwrap(),
                            code_id: lib.code_id.map(|ci| ci.to_string()),
                            arch: lib.arch.map(ToOwned::to_owned),
                            symbol_table: None,
                        });
                        self.lib_mapping_ops.push(
                            now_mono,
                            LibMappingOp::Add(LibMappingAdd {
                                start_avma: lib.base_avma,
                                end_avma: lib.base_avma + lib.vmsize,
                                relative_address_at_start: 0,
                                info: LibMappingInfo::new_lib(lib_handle),
                            }),
                        );
                    }
                }
                Modification::Removed(lib) => {
                    self.unwinder.remove_module(lib.base_avma);
                    self.lib_mapping_ops.push(
                        now_mono,
                        LibMappingOp::Remove(LibMappingRemove {
                            start_avma: lib.base_avma,
                        }),
                    );
                }
            }
        }
    }

    fn add_lib_to_unwinder_and_ensure_debug_id(&mut self, lib: &mut DyldInfo) {
        let base_svma = lib.svma_info.base_svma;
        let base_avma = lib.base_avma;
        let unwind_info_data = lib
            .unwind_sections
            .unwind_info_section
            .and_then(|(svma, size)| {
                VmSubData::map_from_task(self.task, svma - base_svma + base_avma, size).ok()
            });
        let eh_frame_data = lib
            .unwind_sections
            .eh_frame_section
            .and_then(|(svma, size)| {
                VmSubData::map_from_task(self.task, svma - base_svma + base_avma, size).ok()
            });
        let text_data = lib.unwind_sections.text_segment.and_then(|(svma, size)| {
            let avma = svma - base_svma + base_avma;
            VmSubData::map_from_task(self.task, avma, size)
                .ok()
                .map(|data| {
                    TextByteData::new(UnwindSectionBytes::Remapped(data), avma..avma + size)
                })
        });

        if lib.debug_id.is_none() {
            if let (Some(text_data), Some(text_section)) =
                (text_data.as_ref(), lib.svma_info.text.clone())
            {
                let text_section_start_avma = text_section.start - base_svma + base_avma;
                let text_section_first_page_end_avma = text_section_start_avma.wrapping_add(4096);
                let debug_id = if let Some(text_first_page) =
                    text_data.get_bytes(text_section_start_avma..text_section_first_page_end_avma)
                {
                    // Generate a debug ID from the __text section.
                    DebugId::from_text_first_page(text_first_page, true)
                } else {
                    DebugId::nil()
                };
                lib.debug_id = Some(debug_id);
            }
        }

        let unwind_data = match (unwind_info_data, eh_frame_data) {
            (Some(unwind_info), eh_frame) => ModuleUnwindData::CompactUnwindInfoAndEhFrame(
                UnwindSectionBytes::Remapped(unwind_info),
                eh_frame.map(UnwindSectionBytes::Remapped),
            ),
            (None, Some(eh_frame)) => {
                ModuleUnwindData::EhFrame(UnwindSectionBytes::Remapped(eh_frame))
            }
            (None, None) => {
                // Have no unwind information.
                // Let's try to open the file and use debug_frame.
                if let Some(debug_frame) = get_debug_frame(&lib.file) {
                    ModuleUnwindData::DebugFrame(debug_frame)
                } else {
                    ModuleUnwindData::None
                }
            }
        };

        let module = Module::new(
            lib.file.clone(),
            lib.base_avma..(lib.base_avma + lib.vmsize),
            lib.base_avma,
            lib.svma_info.clone(),
            unwind_data,
            text_data,
        );
        self.unwinder.add_module(module);
    }

    pub fn check_jitdump(
        &mut self,
        profile: &mut Profile,
        jit_category_manager: &mut JitCategoryManager,
    ) {
        while let Ok(jitdump_path) = self.jitdump_path_receiver.try_recv() {
            self.jitdump_manager.add_jitdump_path(jitdump_path, None);
        }

        self.jitdump_manager.process_pending_records(
            jit_category_manager,
            profile,
            self.jit_function_recycler.as_mut(),
            &self.timestamp_converter,
        );
    }

    /// Called when a process has exited, before finish(). Not called if the process
    /// is still alive at the end of the profiling run.
    pub fn notify_dead(&mut self, end_time: Timestamp, profile: &mut Profile) {
        for (_, mut thread) in self.live_threads.drain() {
            thread.notify_dead(end_time, profile);
            let (thread_name, thread_handle) = thread.finish();

            if let (Some(thread_name), Some(thread_recycler)) =
                (thread_name, self.thread_recycler.as_mut())
            {
                thread_recycler.add_to_pool(&thread_name, thread_handle);
            }
        }
        profile.set_process_end_time(self.profile_process, end_time);
        self.lib_info_manager.unmap_memory();
    }

    /// Called when a process has exited or at the end of the profiling run.
    pub fn finish(
        mut self,
        jit_category_manager: &mut JitCategoryManager,
        profile: &mut Profile,
    ) -> (ProcessSampleData, Option<(String, ProcessRecyclingData)>) {
        let perf_map_mappings = if !self.unresolved_samples.is_empty() {
            try_load_perf_map(self.pid, profile, jit_category_manager, None)
        } else {
            None
        };
        let jitdump_lib_ops = self.jitdump_manager.finish(
            jit_category_manager,
            profile,
            self.jit_function_recycler.as_mut(),
            &self.timestamp_converter,
        );
        let process_sample_data = ProcessSampleData::new(
            self.unresolved_samples,
            self.lib_mapping_ops,
            jitdump_lib_ops,
            perf_map_mappings,
        );

        let recycling_data = if let (Some(mut jit_function_recycler), Some(thread_recycler)) =
            (self.jit_function_recycler, self.thread_recycler)
        {
            jit_function_recycler.finish_round();
            Some((
                self.executable_name,
                ProcessRecyclingData {
                    process_handle: self.profile_process,
                    main_thread_handle: self.main_thread_handle,
                    thread_recycler,
                    jit_function_recycler,
                },
            ))
        } else {
            None
        };

        (process_sample_data, recycling_data)
    }
}

fn get_debug_frame(file_path: &str) -> Option<UnwindSectionBytes> {
    let file = std::fs::File::open(file_path).ok()?;
    let mmap = unsafe { memmap2::MmapOptions::new().map(&file).ok()? };
    let data = &mmap[..];
    let obj = object::read::File::parse(data).ok()?;
    let compressed_range = if let Some(zdebug_frame_section) = obj.section_by_name("__zdebug_frame")
    {
        // Go binaries use compressed sections of the __zdebug_* type even on macOS,
        // where doing so is quite uncommon. Object's mach-O support does not handle them.
        // But we want to handle them.
        let (file_range_start, file_range_size) = zdebug_frame_section.file_range()?;
        let section_data = zdebug_frame_section.data().ok()?;
        if !section_data.starts_with(b"ZLIB\0\0\0\0") {
            return None;
        }
        let b = section_data.get(8..12)?;
        let uncompressed_size = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        CompressedFileRange {
            format: CompressionFormat::Zlib,
            offset: file_range_start + 12,
            compressed_size: file_range_size - 12,
            uncompressed_size: uncompressed_size.into(),
        }
    } else {
        let debug_frame_section = obj.section_by_name("__debug_frame")?;
        debug_frame_section.compressed_file_range().ok()?
    };
    match compressed_range.format {
        CompressionFormat::None => Some(UnwindSectionBytes::Mmap(MmapSubData::try_new(
            mmap,
            compressed_range.offset as usize,
            compressed_range.uncompressed_size as usize,
        )?)),
        CompressionFormat::Unknown => None,
        CompressionFormat::Zlib => {
            let compressed_bytes = &mmap[compressed_range.offset as usize..]
                [..compressed_range.compressed_size as usize];

            let mut decompressed = Vec::with_capacity(compressed_range.uncompressed_size as usize);
            let mut decompress = flate2::Decompress::new(true);
            decompress
                .decompress_vec(
                    compressed_bytes,
                    &mut decompressed,
                    flate2::FlushDecompress::Finish,
                )
                .ok()?;
            Some(UnwindSectionBytes::Allocated(decompressed))
        }
        _ => None,
    }
}

fn get_thread_list(task: mach_port_t) -> Result<Vec<thread_act_t>, SamplingError> {
    let mut thread_list: thread_act_port_array_t = std::ptr::null_mut();
    let mut thread_count: mach_msg_type_number_t = Default::default();
    unsafe { task_threads(task, &mut thread_list, &mut thread_count) }
        .into_result()
        .map_err(|err| match err {
            KernelError::InvalidArgument
            | KernelError::MachSendInvalidDest
            | KernelError::Terminated => {
                SamplingError::ProcessTerminated("task_threads in get_thread_list", err)
            }
            err => SamplingError::Ignorable("task_threads in get_thread_list", err),
        })?;

    let thread_acts =
        unsafe { std::slice::from_raw_parts(thread_list, thread_count as usize) }.to_owned();

    unsafe {
        mach_vm_deallocate(
            mach_task_self(),
            thread_list as usize as mach_vm_address_t,
            (thread_count as usize * mem::size_of::<thread_act_t>()) as mach_vm_size_t,
        )
    }
    .into_result()
    .map_err(|err| SamplingError::Fatal("mach_vm_deallocate in get_thread_list", err))?;

    Ok(thread_acts)
}
