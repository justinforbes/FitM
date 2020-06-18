use std::process::{Command, Child, Stdio};
use std::path::Path;
use std::fs;
use std::fmt;
use std::io;
use std::env;
use std::collections::{VecDeque, BTreeSet};
use std::os::unix::fs::OpenOptionsExt;

// client_set: set of afl-showmap on client outputs that are relevant for us
// server_set: set of afl-showmap on server outputs that are relevant for us

fn mv(from: String, to: String) {
    Command::new("mv").args(&[
        from.clone(),
        to.clone()
    ])
    .spawn()
    .expect("[!] Could not start moving dirs")
    .wait()
    .expect(format!("[!] Moving dir failed To: {} From: {}", to, from)
    .as_str());
}

fn copy(from: String, to: String) {
    Command::new("cp").args(&[
        String::from("-r"),
        from.clone(),
        to.clone()
    ])
    .spawn()
    .expect("[!] Could not start copying dirs")
    .wait()
    .expect(format!("[!] Copying dir failed To: {} From: {}", to, from)
        .as_str());
}

fn rm(target: String) {
    let _ = Command::new("rm").args(&[
        format!("-rf"),
        format!("./active-state/{}", target),
    ])
        .spawn()
        .expect("[!] Could not start removing active-states folders")
        .wait()
        .expect("[!] Removing state folder from active-state failed");
}

/// AFLRun contains all the information for one specific fuzz run.
#[derive(Clone)]
struct AFLRun {
    /// Path to the base directory of the state of the current fuzz run
    state_path: String,
    /// Binary that is being fuzzed
    target_bin: String,
    /// Path to the state the current state receives input from
    previous_state_path: String,
    /// Timeout for this run
    /// TODO: probably should be dynamic based on how interesting this state is.
    timeout: u32,
    // All the states that came out of the current state
    // child_states: Vec<(u32, u32)>
    /// Used to determine whether to increase first or second value of state
    /// tuple. Hope this is not too broken
    server: bool
}

impl fmt::Debug for AFLRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AFLRun")
            .field("state_path", &self.state_path)
            .field("target_bin", &self.target_bin)
            .field("previous_state_path", &self.previous_state_path)
            .field("timeout", &self.timeout)
            .field("server", &self.server)
            .finish()
    }
}

/// Implementation of functions for an afl run
impl AFLRun {
    /// Create a new afl run instance
    fn new(state_path: String, target_bin: String, timeout: u32,
           previous_state_path: String, server: bool) -> AFLRun {
        // If the new state directory already exists we may have old data there
        // so we optionally delete it
        if Path::new(&format!("active-state/{}", state_path)).exists() {
            println!("[!] active-state/{} already exists! Recreating..",
                     state_path);
            let delete = true;
            if delete {
                // expect already panics so we don't need to exit manually
                fs::remove_dir(format!("active-state/{}", state_path))
                    .expect("[-] Could not remove duplicate state dir!");
            }
        }

        // Create the new directories and files to make afl feel at home
        fs::create_dir(format!("active-state/{}", state_path))
            .expect("[-] Could not create state dir!");

        fs::create_dir(format!("active-state/{}/in", state_path))
            .expect("[-] Could not create in dir!");

        fs::create_dir(format!("active-state/{}/out", state_path))
            .expect("[-] Could not create out dir!");

        fs::create_dir(format!("active-state/{}/out/maps", state_path))
            .expect("[-] Could not create out/maps dir!");

        fs::create_dir(format!("active-state/{}/fd", state_path))
            .expect("[-] Could not create fd dir!");

        fs::create_dir(format!("active-state/{}/snapshot", state_path))
            .expect("[-] Could not create snapshot dir!");

        // Create a dummy .cur_input because the file has to exist once criu
        // restores the process
        fs::OpenOptions::new()
            .create(true)
            .write(true)
            .mode(0o600)
            .open(format!("active-state/{}/out/.cur_input", state_path))
            .unwrap();

        AFLRun{ 
            state_path,
            target_bin,
            timeout,
            previous_state_path,
            server
        }
    }

    /// Needed for the two initial snapshots created based on the target binaries
    fn init_run(&self) -> () {
        // create the .cur_input so that criu snapshots a fd connected to
        // .cur_input
        let stdin = fs::File::open(format!("active-state/{}/out/.cur_input",
                                           self.state_path)).unwrap();

        // Change into our state directory and create the snapshot from there
        env::set_current_dir(format!("./active-state/{}", self.state_path))
            .unwrap();

        // Open a file for stdout and stderr to log to
        let stdout = fs::File::create("stdout").unwrap();
        let stderr = fs::File::create("stderr").unwrap();

        // Start the initial snapshot run. We use our patched qemu to emulate
        // until the first recv of the target is hit. We have to use setsid to
        // circumvent the --shell-job problem of criu and stdbuf to have the
        // correct stdin, stdout and stderr file descriptors.
        let _ = Command::new("setsid")
            .args(&[
                format!("stdbuf"),
                format!("-oL"),
                format!("../../AFLplusplus/afl-qemu-trace"),
                format!("../../{}", self.target_bin),
            ])
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .env("LETS_DO_THE_TIMEWARP_AGAIN", "1")
            .env("CRIU_SNAPSHOT_DIR", "./snapshot")
            .env("AFL_NO_UI", "1")
            .spawn()
            .expect("[!] Could not spawn snapshot run")
            .wait()
            .expect("[!] Snapshot run failed");


        // After spawning the run we go back into the base directory
        env::set_current_dir(&Path::new("../../")).unwrap();

        mv(format!("./active-state/{}", self.state_path),
           String::from("./saved-states/"));
    }

    /// Create a new snapshot based on a given snapshot
    fn snapshot_run(&self, stdin: String) -> () {
        let stdin_file = fs::File::open(stdin.clone()).unwrap();
        // Change into our state directory and create the snapshot from there
        env::set_current_dir(format!("./active-state/{}", self.state_path))
            .unwrap();

        // Open a file for stdout and stderr to log to
        let stdout = fs::File::create("stdout").unwrap();
        let stderr = fs::File::create("stderr").unwrap();

        // Start the initial snapshot run. We use our patched qemu to emulate
        // until the first recv of the target is hit. We have to use setsid to
        // circumvent the --shell-job problem of criu and stdbuf to have the
        // correct stdin, stdout and stderr file descriptors.
        let _ = Command::new("setsid")
            .args(&[
                format!("stdbuf"),
                format!("-oL"),
                format!("../../restore.sh"),
                format!("../../{}", self.state_path),
                stdin,
            ])
            .stdin(Stdio::from(stdin_file))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .env("LETS_DO_THE_TIMEWARP_AGAIN", "1")
            .env("CRIU_SNAPSHOT_DIR", "./snapshot")
            .env("AFL_NO_UI", "1")
            .spawn()
            .expect("[!] Could not spawn snapshot run")
            .wait()
            .expect("[!] Snapshot run failed");

        // After spawning the run we go back into the base directory
        env::set_current_dir(&Path::new("../../")).unwrap();

        mv(format!("./active-state/{}", self.state_path),
           String::from("./saved-states/"));
    }

    /// Start a single fuzz run in afl which gets restored from an earlier
    /// snapshot. Because we use sh and the restore script we have to skip the
    /// bin check
    fn fuzz_run(&self) -> io::Result<Child> {
        copy(format!("./saved-states/{}", self.state_path),
             String::from("./active-state/"));

        // Change into our state directory and create fuzz run from there
        env::set_current_dir(format!("./active-state/{}", self.state_path))
            .unwrap();

        // Spawn the afl run in a command. This run is relative to the state dir
        // meaning we already are inside the directory. This prevents us from
        // accidentally using different resources than we expect.
        let ret = Command::new("../../AFLplusplus/afl-fuzz")
            .args(&[
                format!("-i"),
                format!("./in"),
                format!("-o"),
                format!("./out"),
                format!("-m"),
                format!("none"),
                format!("-d"),
                format!("-V"),
                format!("{}", self.timeout),
                format!("--"),
                format!("sh"),
                format!("../../restore.sh"),
                format!("{}", self.state_path),
                format!("@@")
            ])
            .env("CRIU_SNAPSHOT_DIR", "./snapshot")
            .env("AFL_SKIP_BIN_CHECK", "1")
            .env("AFL_NO_UI", "1")
            .spawn();

        // After spawning the run we go back into the base directory
        env::set_current_dir(&Path::new("../../")).unwrap();

        ret
    }

    /// Generate the maps provided by afl-showmap. This is used to filter out 
    /// for "interesting" new seeds meaning seeds, that will make the OTHER 
    /// binary produce paths, which we haven't seen yet.
    fn gen_afl_maps(&self) -> io::Result<Child> {
        copy(format!("./saved-states/{}", self.previous_state_path),
             String::from("./active-state/"));

        // Change into our state directory and generate the afl maps there
        env::set_current_dir(format!("./active-state/{}", self.state_path))
            .unwrap();

        // Execute afl-showmap from the state dir. We take all the possible 
        // inputs for the OTHER binary that we created with a call to `send`.
        // We then save the generated maps inside `out/maps` where they are used
        // later.
        // For the first run fitm-c1s0 "previous_state_path" actually is the
        // upcoming state.
        let ret = Command::new("../../AFLplusplus/afl-showmap")
            .args(&[
                format!("-i"),
                format!("./fd"),
                format!("-o"),
                format!("./out/maps"),
                format!("-m"),
                format!("none"),
                format!("-Q"),
                format!("--"),
                format!("sh"),
                format!("../../restore.sh"),
                format!("{}", self.previous_state_path),
                format!("@@")
            ])
            .env("CRIU_SNAPSHOT_DIR", "./snapshot")
            .env("AFL_SKIP_BIN_CHECK", "1")
            .env("AFL_NO_UI", "1")
            .env("AFL_DEBUG", "1")
            .spawn();

        // After spawning showmap command we go back into the base directory
        env::set_current_dir(&Path::new("../../")).unwrap();

        ret
    }

    fn create_new_run(&self, new_state: (u32, u32), input: String, timeout: u32)
                      -> AFLRun {
        let input_path: String = format!("active-state/{}/fd/{}",
                                         self.state_path, input);

        let target_bin = if self.server{
            "test/pseudoclient".to_string()
        } else {
            "test/pseudoserver".to_string()
        };

        // Only mutate cur_state in this method. So next_state_path gets a
        // readable copy. We update cur_state here with a new tuple.
        // cur_state = next_state_path(cur_state, true);
        let afl = AFLRun::new(
            format!("fitm-c{}s{}", new_state.0, new_state.1),
            target_bin.to_string(),
            timeout,
            self.state_path.clone(),
            !self.server
        );

        let seed_file_path = format!("active-state/{}/in/{}", afl.state_path,
                                     input);

        fs::copy(input_path, &seed_file_path)
            .expect("[!] Could not copy to new afl.state_path");

        // let seed_file = fs::File::open(seed_file_path)
        //     .expect("[!] Could not create input file");

        afl.snapshot_run(seed_file_path);



        afl
    }
}

/// Create the next iteration from a given state directory. If inc_server is set
/// we will increment the state for the server from fitm-cXsY to fitm-cXsY+1.
/// Otherwise we will increment the state for the client from fitm-cXsY to
/// fitm-cX+1sY
fn next_state_path(state_path: (u32, u32), cur_is_server: bool) -> (u32, u32) {
    // If inc_server increment the server state else increment the client state
    if cur_is_server {
        ((state_path.0)+1, state_path.1)
    } else {
        (state_path.0, (state_path.1)+1)
    }
}



pub fn run() {
    let cur_timeout = 1;
    let mut cur_state: (u32, u32) = (1, 0);
    let mut client_maps: BTreeSet<String> = BTreeSet::new();

    let afl_client: AFLRun = AFLRun::new(
        "fitm-c1s0".to_string(),
        "test/pseudoclient".to_string(),
        cur_timeout,
        // TODO: Need some extra handling for this previous_path value
        "".to_string(),
        false
    );

    let afl_server: AFLRun = AFLRun::new(
        "fitm-c0s1".to_string(),
        "test/pseudoserver".to_string(),
        cur_timeout,
        "fitm-c1s0".to_string(),
        true
    );
    let mut queue: VecDeque<AFLRun> = VecDeque::new();

    fs::write(format!("active-state/{}/in/1", afl_client.state_path),
              "init case.").expect("[-] Could not create initial test case!");

    afl_server.init_run();
    afl_client.init_run();

    queue.push_back(afl_client);
    queue.push_back(afl_server);
    // this does not terminate atm as consolidate_poc does not yet minimize
    // anything
    while !queue.is_empty() {
        // kick off new run
        let afl_current = queue.pop_front().unwrap();

        println!("[*] Starting the fuzz run of: {}", afl_current.state_path);

        if afl_current.previous_state_path != "".to_string() ||
            afl_current.state_path != "fitm-c0s1".to_string() {
            let mut child_fuzz = afl_current.fuzz_run()
                .expect("[!] Failed to start fuzz run");

            child_fuzz.wait().expect("[!] Error while waiting for fuzz run");
        }

        // TODO: Fancier solution? Is this correct?
        println!("[*] Generating maps for: {}", afl_current.state_path);
        if afl_current.previous_state_path != "".to_string() {
            let mut child_map = afl_current.gen_afl_maps()
                .expect("[!] Failed to start the showmap run");

            child_map.wait().expect("[!] Error while waiting for the showmap run");
        } else {
            // copy output of first run of binary 1 to in of first run of bin 2 as seed
            // apparently fs_extra can not copy content of `from` into folder `[..]/in`
            let from = format!("active-state/{}/fd", afl_current.state_path);
            for entry in fs::read_dir(from)
                .expect("[!] Could not read output of initial run") {
                let entry_path = entry.unwrap().path();
                let filename = entry_path.file_name().unwrap().to_string_lossy();
                let to = format!("saved-states/{}/in/{}", "fitm-c0s1", filename);

                std::fs::copy(entry_path, to).unwrap();
            }
        }

        // consolidate previous runs here
        let path = format!("active-state/{}/out/maps", afl_current.state_path);

        for entry in fs::read_dir(path)
            .expect("[!] Could not read maps dir while consolidating") {
            let entry_path = entry.unwrap().path();
            let new_map = fs::read_to_string(entry_path.clone())
                .expect("[!] Could not read map file while consolidating");
                
            if !client_maps.contains(new_map.as_str()) {
                client_maps.insert(new_map);

                // Consolidating binary 1 will yield more runs on binary 2
                cur_state = next_state_path(cur_state, afl_current.server);

                let in_file = entry_path.file_name().unwrap().to_str().unwrap();

                // if afl_current == first binary, first run
                let next_run = if afl_current.previous_state_path == ""
                        .to_string() {
                    let tmp = queue.pop_front()
                        .expect("[!] Could not get second afl_run from queue");

                    let from = format!("active-state/{}/fd/{}",
                        afl_current.state_path, in_file);
                    let to   = format!("saved-states/{}/in/{}", tmp.state_path,
                        in_file);
                    
                    fs::copy(from, to)
                        .expect("[!] Could not copy in file to new state");

                    queue.push_front(tmp.clone());

                    None
                } else {
                    Some(afl_current.create_new_run(cur_state,
                                                    String::from(in_file),
                                                    afl_current.timeout.into()))
                };

                if let Some(next_run) = next_run {
                    queue.push_back(next_run);
                }

                rm(afl_current.state_path.clone());
                rm(afl_current.previous_state_path.clone());
            } else {
                rm(afl_current.state_path.clone());
            }

            //.TODO: Change to a variable like `init-state`
            if afl_current.state_path != "fitm-c0s1".to_string() {
                queue.push_back(afl_current.clone());
            }
        }
    }

    println!("[*] Reached end of programm. Quitting.");
}
