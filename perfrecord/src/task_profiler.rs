use super::kernel_error::{self, IntoResult, KernelError};
use super::proc_maps::{DyldInfo, DyldInfoManager, Modification};
use super::thread_profiler::ThreadProfiler;
use mach::mach_types::thread_act_port_array_t;
use mach::mach_types::thread_act_t;
use mach::message::mach_msg_type_number_t;
use mach::port::mach_port_t;
use mach::task::task_threads;
use mach::traps::mach_task_self;
use mach::vm::mach_vm_deallocate;
use mach::vm_types::{mach_vm_address_t, mach_vm_size_t};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::mem;
use std::path::Path;
use std::time::{Duration, Instant};

use super::gecko_profile::ProfileBuilder;

pub struct TaskProfiler {
    task: mach_port_t,
    pid: u32,
    interval: Duration,
    start_time: Instant,
    end_time: Option<Instant>,
    live_threads: HashMap<thread_act_t, ThreadProfiler>,
    dead_threads: Vec<ThreadProfiler>,
    lib_info_manager: DyldInfoManager,
    libs: Vec<DyldInfo>,
    commandline: Option<Vec<String>>,
    executable_lib: Option<DyldInfo>,
    command_name: String,
}

impl TaskProfiler {
    pub fn new(
        task: mach_port_t,
        pid: u32,
        now: Instant,
        command_name: &str,
        interval: Duration,
    ) -> kernel_error::Result<Self> {
        let process = remoteprocess::Process {
            pid: pid as i32,
            task,
        };
        let commandline = match process.cmdline() {
            Ok(mut cmds) => {
                if let Some(command) = cmds.first_mut() {
                    // Strip off path.
                    *command = Path::new(command)
                        .file_name()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_string();
                }
                Some(cmds)
            }
            Err(_) => None,
        };
        let thread_acts = get_thread_list(task)?;
        let mut live_threads = HashMap::new();
        for (i, thread_act) in thread_acts.into_iter().enumerate() {
            // Pretend that the first thread is the main thread. Might not be true.
            let is_main = i == 0;
            if let Some(thread) = ThreadProfiler::new(task, pid, now, thread_act, now, is_main)? {
                live_threads.insert(thread_act, thread);
            }
        }
        Ok(TaskProfiler {
            task,
            pid,
            interval,
            start_time: now,
            end_time: None,
            live_threads,
            dead_threads: Vec::new(),
            lib_info_manager: DyldInfoManager::new(task),
            libs: Vec::new(),
            command_name: command_name.to_owned(),
            commandline,
            executable_lib: None,
        })
    }

    pub fn sample(&mut self, now: Instant) -> kernel_error::Result<bool> {
        let result = self.sample_impl(now);
        match result {
            Ok(()) => Ok(true),
            Err(KernelError::MachSendInvalidDest) => Ok(false),
            Err(KernelError::Terminated) => Ok(false),
            Err(err) => Err(err),
        }
    }

    fn sample_impl(&mut self, now: Instant) -> kernel_error::Result<()> {
        // First, check for any newly-loaded libraries.
        let changes = self
            .lib_info_manager
            .check_for_changes()
            .unwrap_or_else(|_| Vec::new());
        for change in changes {
            match change {
                Modification::Added(lib) => {
                    if self.executable_lib.is_none() && lib.is_executable {
                        self.executable_lib = Some(lib.clone());
                    }
                    self.libs.push(lib)
                }
                Modification::Removed(_) => {
                    // Ignore, and hope that the address ranges won't be reused by other libraries
                    // during the rest of the recording...
                }
            }
        }

        // Enumerate threads.
        let thread_acts = get_thread_list(self.task).map_err(|err| match err {
            KernelError::InvalidArgument => KernelError::Terminated,
            err => err,
        })?;
        let previously_live_threads: HashSet<_> =
            self.live_threads.iter().map(|(t, _)| *t).collect();
        let mut now_live_threads = HashSet::new();
        for thread_act in thread_acts {
            let mut entry = self.live_threads.entry(thread_act);
            let thread = match entry {
                Entry::Occupied(ref mut entry) => entry.get_mut(),
                Entry::Vacant(entry) => {
                    match ThreadProfiler::new(
                        self.task,
                        self.pid,
                        self.start_time,
                        thread_act,
                        now,
                        false,
                    )? {
                        Some(thread) => entry.insert(thread),
                        None => continue,
                    }
                }
            };
            // Grab a sample from the thread.
            let still_alive = thread.sample(now)?;
            if still_alive {
                now_live_threads.insert(thread_act);
            }
        }
        let dead_threads = previously_live_threads.difference(&now_live_threads);
        for thread_act in dead_threads {
            let mut thread = self.live_threads.remove(thread_act).unwrap();
            thread.notify_dead(now);
            self.dead_threads.push(thread);
        }
        Ok(())
    }

    pub fn notify_dead(&mut self, end_time: Instant) {
        for (_, mut thread) in self.live_threads.drain() {
            thread.notify_dead(end_time);
            self.dead_threads.push(thread);
        }
        self.end_time = Some(end_time);
        self.lib_info_manager.unmap_memory();
    }

    pub fn into_profile(self, subtasks: Vec<TaskProfiler>) -> ProfileBuilder {
        let executable_lib = self.executable_lib;
        let name = self
            .commandline
            .map(|cmds| cmds.join(" "))
            .or_else(|| {
                executable_lib.map(|l| {
                    Path::new(&l.file)
                        .file_name()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_string()
                })
            })
            .unwrap_or(self.command_name);

        let mut profile_builder =
            ProfileBuilder::new(self.start_time, &name, self.pid, self.interval);
        let all_threads = self
            .live_threads
            .into_iter()
            .map(|(_, t)| t)
            .chain(self.dead_threads.into_iter())
            .map(|t| t.into_profile_thread());
        for thread in all_threads {
            profile_builder.add_thread(thread);
        }

        if let Some(end_time) = self.end_time {
            profile_builder.set_end_time(end_time.duration_since(self.start_time));
        }

        for DyldInfo {
            file,
            uuid,
            address,
            vmsize,
            arch,
            ..
        } in self.libs
        {
            let (uuid, arch) = match (uuid, arch) {
                (Some(uuid), Some(arch)) => (uuid, arch),
                _ => continue,
            };
            let name = Path::new(&file).file_name().unwrap().to_str().unwrap();
            let address_range = address..(address + vmsize);
            profile_builder.add_lib(name, &file, &uuid, arch, &address_range);
        }

        for subtask in subtasks {
            profile_builder.add_subprocess(subtask.into_profile(Vec::new()));
        }

        profile_builder
    }
}

fn get_thread_list(task: mach_port_t) -> kernel_error::Result<Vec<thread_act_t>> {
    let mut thread_list: thread_act_port_array_t = std::ptr::null_mut();
    let mut thread_count: mach_msg_type_number_t = Default::default();
    unsafe { task_threads(task, &mut thread_list, &mut thread_count) }.into_result()?;

    let thread_acts =
        unsafe { std::slice::from_raw_parts(thread_list, thread_count as usize) }.to_owned();

    unsafe {
        mach_vm_deallocate(
            mach_task_self(),
            thread_list as usize as mach_vm_address_t,
            (thread_count as usize * mem::size_of::<thread_act_t>()) as mach_vm_size_t,
        )
    }
    .into_result()?;

    Ok(thread_acts)
}
