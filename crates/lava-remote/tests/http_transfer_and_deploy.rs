#![cfg(unix)]

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use lava_remote::{
    AsyncPollOutcome, AsyncRemoteOp, AsyncRemoteResult, HTTP_TRANSFER_THRESHOLD_BYTES,
    RemoteManager, TransferProgressSnapshot, discover_ssh_config_hosts_from_path,
    set_simulated_rpc_delay_ms,
};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn find_lava_server_bin() -> PathBuf {
    let root = workspace_root();
    let debug_bin = root.join("target/debug/lava-server");
    if debug_bin.is_file() {
        return debug_bin;
    }
    let release_bin = root.join("target/release/lava-server");
    if release_bin.is_file() {
        return release_bin;
    }
    panic!(
        "lava-server binary not found under {}/target/{{debug,release}}/lava-server; build -p lava-server first",
        root.display()
    );
}

#[test]
fn openssh_include_discovery_ssh_deploy_and_http_large_file_streaming() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp_root = env::temp_dir().join(format!("lava_remote_http_e2e_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(&tmp_root).expect("create temp root");

    // 1. Verify OpenSSH `Include` glob expansion + rich profile discovery.
    let ssh_dir = tmp_root.join("ssh_cfg");
    let conf_d = ssh_dir.join("conf.d");
    fs::create_dir_all(&conf_d).expect("create conf.d");
    fs::write(
        ssh_dir.join("config"),
        "Include conf.d/*.conf\n\nHost staging-api\n  HostName 10.20.0.9\n  User deploy\n  Port 2222\n",
    )
    .expect("write main ssh config");
    fs::write(
        conf_d.join("10-gpu.conf"),
        "Host gpu-cluster *.internal\n  HostName 127.0.0.1\n  User researcher\n  Port 2200\n  ProxyJump bastion\n",
    )
    .expect("write included ssh config");

    let discovered = discover_ssh_config_hosts_from_path(&ssh_dir.join("config"));
    assert_eq!(
        discovered.len(),
        2,
        "expected 2 concrete Host aliases (wildcard skipped)"
    );
    let gpu = discovered
        .iter()
        .find(|h| h.alias == "gpu-cluster")
        .expect("Include glob host discovered");
    assert_eq!(gpu.hostname, "127.0.0.1");
    assert_eq!(gpu.user, "researcher");
    assert_eq!(gpu.port, 2200);
    assert_eq!(gpu.proxy_jump, "bastion");

    // 2. Create a local `ssh` shim script so `RemoteManager` exercises the real SSH CLI spawn path
    //    against localhost with zero external sshd / network dependency.
    let bin_dir = tmp_root.join("fake_bin");
    fs::create_dir_all(&bin_dir).expect("create fake_bin");
    let ssh_shim = bin_dir.join("ssh");
    let shim_script = r#"#!/bin/sh
while [ $# -gt 0 ]; do
    case "$1" in
        -O)
            exit 0
            ;;
        -p|-o|-S|-L|-i|-J|-F|-l)
            shift 2
            ;;
        -N|-T|-t|-q|-v|-C|-f)
            shift 1
            ;;
        -*)
            shift 1
            ;;
        *)
            shift 1
            break
            ;;
    esac
done
exec /bin/sh -c "$*"
"#;
    fs::write(&ssh_shim, shim_script).expect("write ssh shim");
    fs::set_permissions(&ssh_shim, fs::Permissions::from_mode(0o755)).expect("chmod ssh shim");

    let install_dir = tmp_root.join("dot_lava_server");
    let remote_work_dir = tmp_root.join("remote_workspace");
    fs::create_dir_all(&remote_work_dir).expect("create remote_workspace");

    let orig_path = env::var("PATH").unwrap_or_default();
    let orig_install = env::var("LAVA_REMOTE_INSTALL_DIR").ok();
    // SAFETY: Single integration test binary owns its process environment.
    unsafe {
        env::set_var("PATH", format!("{}:{orig_path}", bin_dir.display()));
        env::set_var("LAVA_REMOTE_INSTALL_DIR", &install_dir);
    }

    let server_bin = find_lava_server_bin();
    let mgr = RemoteManager::new();

    let events: Arc<Mutex<Vec<TransferProgressSnapshot>>> = Arc::new(Mutex::new(Vec::new()));
    let events_sink = Arc::clone(&events);
    mgr.set_progress_callback(Some(Arc::new(move |snap_opt| {
        if let Some(snap) = snap_opt
            && let Ok(mut guard) = events_sink.lock()
        {
            guard.push(snap);
        }
    })));

    // Before installing `~/.lava-server`, connecting falls back to `ssh-posix`.
    let initial_info = mgr
        .connect("ssh://127.0.0.1")
        .expect("initial connect via ssh-posix");
    assert_eq!(initial_info.mode, "ssh-posix");

    // 3. Deploy `lava-server` + `server/{packages,extensions}` into `LAVA_REMOTE_INSTALL_DIR` (`~/.lava-server` equivalent)
    //    and verify reconnect upgrades automatically to `"lava-server"` mode!
    let deployed_info = mgr
        .deploy_server(
            "ssh://127.0.0.1",
            server_bin.to_str().expect("utf8 server_bin"),
        )
        .expect("deploy lava-server over ssh shim");
    assert_eq!(
        deployed_info.mode, "lava-server",
        "session must upgrade to lava-server mode launched from ~/.lava-server/lava-server"
    );
    assert!(
        install_dir.join("lava-server").is_file(),
        "installed lava-server binary must exist"
    );
    assert!(
        install_dir
            .join("server/extensions/srv_fs/src/lib.ns")
            .is_file(),
        "synced server Nessa extensions must exist under ~/.lava-server/server"
    );

    // 4. Small file (<= 512 KiB) uses SSH JSON-RPC (`transport == "SSH"`).
    let small_path = remote_work_dir.join("small.rs");
    let small_uri = format!("ssh://127.0.0.1{}", small_path.display());
    let small_payload = b"pub fn answer() -> i64 {\n    42\n}\n";
    let (written_small, small_put_tx) = mgr
        .write_file_bytes(&small_uri, small_payload, false)
        .expect("write small file");
    assert_eq!(written_small as usize, small_payload.len());
    assert_eq!(small_put_tx, "SSH");

    let (ex_s, dir_s, _ro_s, read_small, small_get_tx) =
        mgr.read_file_bytes(&small_uri).expect("read small file");
    assert!(ex_s && !dir_s);
    assert_eq!(small_get_tx, "SSH");
    assert_eq!(read_small, small_payload);

    // Verify `read_preview` returns bounded preview lines without downloading full file.
    let prev = mgr
        .read_preview(&small_uri, 4096, 10)
        .expect("read_preview small.rs");
    assert!(!prev.is_binary);
    assert_eq!(prev.total_size as usize, small_payload.len());
    assert_eq!(prev.lines[0], "pub fn answer() -> i64 {");

    // 5. Large file (> 512 KiB, 120,000 lines, ~4.2 MiB) MUST stream over authenticated HTTP!
    if let Ok(mut guard) = events.lock() {
        guard.clear();
    }
    let mut big_buf = String::with_capacity(120_000 * 36);
    for i in 0..120_000 {
        big_buf.push_str(&format!("pub const ROW_{i:06}: usize = {i};\n"));
    }
    let big_bytes = big_buf.into_bytes();
    assert!(
        (big_bytes.len() as u64) > HTTP_TRANSFER_THRESHOLD_BYTES * 6,
        "payload ({} bytes) must be well above 512 KiB threshold",
        big_bytes.len()
    );

    let big_path = remote_work_dir.join("huge_120k.rs");
    let big_uri = format!("ssh://127.0.0.1{}", big_path.display());

    let (written_big, big_put_tx) = mgr
        .write_file_bytes(&big_uri, &big_bytes, false)
        .expect("HTTP PUT 120k-line file");
    assert_eq!(written_big as usize, big_bytes.len());
    assert_eq!(
        big_put_tx, "HTTP",
        ">512 KiB upload must stream over HTTP instead of SSH stdio"
    );

    let (ex_b, dir_b, _ro_b, fetched_big, big_get_tx) = mgr
        .read_file_bytes(&big_uri)
        .expect("HTTP GET 120k-line file");
    assert!(ex_b && !dir_b);
    assert_eq!(
        big_get_tx, "HTTP",
        ">512 KiB download must stream over HTTP instead of SSH stdio"
    );
    assert_eq!(fetched_big.len(), big_bytes.len());
    assert_eq!(
        fetched_big, big_bytes,
        "120k-line HTTP roundtrip must be byte-exact"
    );

    let recorded = events.lock().expect("lock progress events").clone();
    assert!(
        recorded
            .iter()
            .any(|e| e.active && e.transport == "HTTP" && e.bytes_done < e.bytes_total),
        "expected live incremental HTTP streaming progress callbacks"
    );
    let final_snap = mgr.last_transfer_progress();
    assert!(
        !final_snap.active,
        "final snapshot must mark transfer complete"
    );
    assert_eq!(final_snap.transport, "HTTP");
    assert_eq!(final_snap.bytes_done, big_bytes.len() as u64);
    assert_eq!(final_snap.lines_loaded, 120_000);

    // 6. Verify remote Git RPC helpers (`git_status`, `git_head_lines`, `git_blame_line`) over `lava-server`.
    let _ = Command::new("git")
        .arg("init")
        .current_dir(&remote_work_dir)
        .output();
    let _ = Command::new("git")
        .args(["config", "user.email", "test@lava.dev"])
        .current_dir(&remote_work_dir)
        .output();
    let _ = Command::new("git")
        .args(["config", "user.name", "Lava Test"])
        .current_dir(&remote_work_dir)
        .output();
    let _ = Command::new("git")
        .args(["add", "small.rs"])
        .current_dir(&remote_work_dir)
        .output();
    let _ = Command::new("git")
        .args(["commit", "-m", "initial remote commit"])
        .current_dir(&remote_work_dir)
        .output();
    fs::write(&small_path, "pub fn answer() -> i64 {\n    99\n}\n").expect("modify small.rs");

    let repo_uri = format!("ssh://127.0.0.1{}", remote_work_dir.display());
    let gst = mgr.git_status(&repo_uri).expect("remote git_status");
    assert!(gst.is_repo, "remote directory must be detected as git repo");
    assert!(
        gst.entries
            .iter()
            .any(|e| e.rel_path.ends_with("small.rs") && e.worktree_status == "M"),
        "remote git_status must report modified small.rs: {:?}",
        gst.entries
    );
    let head_lines = mgr
        .git_head_lines(&repo_uri, &small_uri, "HEAD")
        .expect("remote git_head_lines")
        .expect("tracked small.rs at HEAD");
    assert_eq!(head_lines[1], "    42");
    let blame_row0 = mgr
        .git_blame_line(&repo_uri, &small_uri, 0)
        .expect("remote git_blame_line row 0")
        .expect("blame line 0");
    assert!(!blame_row0.commit_short.is_empty());
    assert_eq!(blame_row0.author, "Lava Test");
    assert_eq!(blame_row0.summary, "initial remote commit");

    let blame_row1 = mgr
        .git_blame_line(&repo_uri, &small_uri, 1)
        .expect("remote git_blame_line row 1")
        .expect("blame line 1");
    assert_eq!(blame_row1.author, "Uncommitted");

    let _ = mgr.disconnect("ssh://127.0.0.1");
    // SAFETY: Restore environment variables before exiting.
    unsafe {
        env::set_var("PATH", orig_path);
        match orig_install {
            Some(v) => env::set_var("LAVA_REMOTE_INSTALL_DIR", v),
            None => env::remove_var("LAVA_REMOTE_INSTALL_DIR"),
        }
    }
    let _ = fs::remove_dir_all(&tmp_root);
}

#[test]
fn remote_fish_login_shell_deploy_and_env_var_launch() {
    if !Path::new("/usr/bin/fish").is_file() {
        return;
    }
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp_root = env::temp_dir().join(format!("lava_remote_fish_e2e_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(&tmp_root).expect("create fish temp root");

    // Simulate Windows `Win32-OpenSSH` (`ssh.exe` wrapping any space-containing argv element in
    // outer `"..."`) talking to a remote host whose login shell is `/usr/bin/fish`!
    let bin_dir = tmp_root.join("fish_fake_bin");
    fs::create_dir_all(&bin_dir).expect("create fish_fake_bin");
    let ssh_shim = bin_dir.join("ssh");
    let shim_script = r#"#!/bin/sh
while [ $# -gt 0 ]; do
    case "$1" in
        -O)
            exit 0
            ;;
        -p|-o|-S|-L|-i|-J|-F|-l)
            shift 2
            ;;
        -N|-T|-t|-q|-v|-C|-f)
            shift 1
            ;;
        -*)
            shift 1
            ;;
        *)
            shift 1
            break
            ;;
    esac
done
REMOTE_CMD=""
for a in "$@"; do
    case "$a" in
        \'*\')
            ;;
        *" "*|*"	"*)
            q=$(printf '%s' "$a" | sed 's/"/\\"/g')
            a="\"$q\""
            ;;
    esac
    if [ -z "$REMOTE_CMD" ]; then
        REMOTE_CMD="$a"
    else
        REMOTE_CMD="$REMOTE_CMD $a"
    fi
done
printf 'Welcome to fish, the friendly interactive shell\n'
exec /usr/bin/fish --no-config -c "$REMOTE_CMD"
"#;
    fs::write(&ssh_shim, shim_script).expect("write fish ssh shim");
    fs::set_permissions(&ssh_shim, fs::Permissions::from_mode(0o755)).expect("chmod fish ssh shim");

    let install_dir = tmp_root.join("dot_lava_server_fish");
    let remote_work_dir = tmp_root.join("fish workspace");
    fs::create_dir_all(&remote_work_dir).expect("create fish workspace");

    let orig_path = env::var("PATH").unwrap_or_default();
    let orig_install = env::var("LAVA_REMOTE_INSTALL_DIR").ok();
    // SAFETY: Protected by `ENV_LOCK`.
    unsafe {
        env::set_var("PATH", format!("{}:{orig_path}", bin_dir.display()));
        env::set_var("LAVA_REMOTE_INSTALL_DIR", &install_dir);
    }

    let server_bin = find_lava_server_bin();
    let mgr = RemoteManager::new();

    // 1. Before deploying `~/.lava-server`, `connect` + POSIX fallback operations must succeed under `fish`!
    let init_info = mgr
        .connect("ssh://127.0.0.1")
        .expect("connect via fish login shell before lava-server deploy");
    assert_eq!(init_info.mode, "ssh-posix");
    assert_eq!(init_info.os, "linux");

    let tricky_file = remote_work_dir.join("quote's_and_space.txt");
    let tricky_uri = format!("ssh://127.0.0.1{}", tricky_file.display());
    let payload = b"fish shell env var & quote safety\nline 2\\ok\n";
    mgr.write_file_bytes(&tricky_uri, payload, false)
        .expect("ssh-posix write_file_bytes over fish");
    let (exists, is_dir, _ro, read_back, _) = mgr
        .read_file_bytes(&tricky_uri)
        .expect("ssh-posix read_file_bytes over fish");
    assert!(exists && !is_dir);
    assert_eq!(read_back, payload);

    // Verify `lsp_launch_command` in `ssh-posix` mode executes cleanly through Win32-OpenSSH + `fish`!
    let (_ssh_prog, lsp_ssh_args, _pfx, _root) = mgr
        .lsp_launch_command(
            "ssh://127.0.0.1",
            remote_work_dir.to_str().expect("utf8 work_dir"),
            "pwd",
            &[],
        )
        .expect("lsp_launch_command in ssh-posix mode");
    let fish_lsp_out = Command::new(&ssh_shim)
        .args(&lsp_ssh_args)
        .output()
        .expect("run lsp_launch_command through Win32-OpenSSH + fish shim");
    assert!(
        fish_lsp_out.status.success(),
        "fish failed on lsp_launch_command: {}",
        String::from_utf8_lossy(&fish_lsp_out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&fish_lsp_out.stdout)
            .lines()
            .any(|l| l.trim() == remote_work_dir.display().to_string()),
        "unexpected lsp_launch_command output: {}",
        String::from_utf8_lossy(&fish_lsp_out.stdout)
    );

    // 2. Deploy `lava-server` (`DEST=...; cat > ...$$` + `LAVA_SERVER_CACHE_DIR=... lava-server --stdio`) over `fish`!
    let deployed = mgr
        .deploy_server(
            "ssh://127.0.0.1",
            server_bin.to_str().expect("utf8 server_bin"),
        )
        .expect("deploy_server and reconnect over fish login shell");
    assert_eq!(
        deployed.mode, "lava-server",
        "deploy_server must succeed and launch lava-server with LAVA_SERVER_CACHE_DIR under fish"
    );
    assert!(install_dir.join("lava-server").is_file());
    assert!(install_dir.join("cache").is_dir());

    // 3. Roundtrip RPC file operations over the live `fish`-launched `lava-server`!
    let srv_file = remote_work_dir.join("via_server.rs");
    let srv_uri = format!("ssh://127.0.0.1{}", srv_file.display());
    let srv_payload = b"pub fn fish_ok() -> bool { true }\n";
    let (written, tx_put) = mgr
        .write_file_bytes(&srv_uri, srv_payload, false)
        .expect("lava-server write over fish");
    assert_eq!(written as usize, srv_payload.len());
    assert_eq!(tx_put, "SSH");
    let (ex2, dir2, _, fetched, tx_get) = mgr
        .read_file_bytes(&srv_uri)
        .expect("lava-server read over fish");
    assert!(ex2 && !dir2);
    assert_eq!(tx_get, "SSH");
    assert_eq!(fetched, srv_payload);

    // 4. Verify `lsp_launch_command` in `--lsp-proxy` mode over Win32-OpenSSH + `fish`!
    let (_ssh_prog2, lsp_proxy_args, _pfx2, _root2) = mgr
        .lsp_launch_command(
            "ssh://127.0.0.1",
            remote_work_dir.to_str().expect("utf8 work_dir"),
            "pwd",
            &[],
        )
        .expect("lsp_launch_command in lava-server --lsp-proxy mode");
    let proxy_out = Command::new(&ssh_shim)
        .args(&lsp_proxy_args)
        .output()
        .expect("run --lsp-proxy through Win32-OpenSSH + fish shim");
    assert!(
        proxy_out.status.success(),
        "fish failed on --lsp-proxy: {}",
        String::from_utf8_lossy(&proxy_out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&proxy_out.stdout)
            .lines()
            .any(|l| l.trim() == remote_work_dir.display().to_string()),
        "unexpected --lsp-proxy output: {}",
        String::from_utf8_lossy(&proxy_out.stdout)
    );

    let _ = mgr.disconnect("ssh://127.0.0.1");
    // SAFETY: Restore environment variables under `ENV_LOCK`.
    unsafe {
        env::set_var("PATH", orig_path);
        match orig_install {
            Some(v) => env::set_var("LAVA_REMOTE_INSTALL_DIR", v),
            None => env::remove_var("LAVA_REMOTE_INSTALL_DIR"),
        }
    }
    let _ = fs::remove_dir_all(&tmp_root);
}

#[test]
fn server_side_12k_index_render_slices_and_async_coalescing() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp_root = env::temp_dir().join(format!("lava_remote_12k_slice_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(&tmp_root).expect("create temp root");

    let bin_dir = tmp_root.join("fake_bin");
    fs::create_dir_all(&bin_dir).expect("create fake_bin");
    let ssh_shim = bin_dir.join("ssh");
    let shim_script = r#"#!/bin/sh
while [ $# -gt 0 ]; do
    case "$1" in
        -O) exit 0 ;;
        -p|-o|-S|-L|-i|-J|-F|-l) shift 2 ;;
        -N|-T|-t|-q|-v|-C|-f) shift 1 ;;
        -*) shift 1 ;;
        *) shift 1; break ;;
    esac
done
exec /bin/sh -c "$*"
"#;
    fs::write(&ssh_shim, shim_script).expect("write ssh shim");
    fs::set_permissions(&ssh_shim, fs::Permissions::from_mode(0o755)).expect("chmod ssh shim");

    let install_dir = tmp_root.join("dot_lava_server");
    let ws_dir = tmp_root.join("huge_repo");
    fs::create_dir_all(&ws_dir).expect("create huge_repo");

    // Seed 12,050 files across 25 buckets + `z_tail/deep_needle_12049.rs` (past old 2k/2.5k/3k/10k caps!)
    for b in 0..25 {
        let bdir = ws_dir.join(format!("bucket_{b:02}"));
        fs::create_dir_all(&bdir).expect("create bucket dir");
        for k in 0..482 {
            let idx = b * 482 + k;
            fs::write(
                bdir.join(format!("mod_{idx:05}.rs")),
                format!("pub const VAL_{idx}: usize = {idx};\n"),
            )
            .expect("write bucket file");
        }
    }
    let tail_dir = ws_dir.join("z_tail");
    fs::create_dir_all(&tail_dir).expect("create z_tail");
    fs::write(
        tail_dir.join("deep_needle_12049.rs"),
        "// TODO: verify uncapped server ranking\npub fn needle_target() -> i32 { 12049 }\n",
    )
    .expect("write deep needle");

    // Seed a 15,000-line source file to verify bounded viewport slice preview (`start_row = 12494`).
    let mut big_lines = String::with_capacity(480_000);
    for r in 0..15_000 {
        if r == 12_500 {
            big_lines.push_str("pub fn hit_here() { /* DEEP_ROW_12500_MARKER */ }\n");
        } else {
            big_lines.push_str(&format!("let row_{r} = {r};\n"));
        }
    }
    let big_file = ws_dir.join("tall_source.rs");
    fs::write(&big_file, &big_lines).expect("write tall_source.rs");

    // Initialize git repo and modify `z_tail/deep_needle_12049.rs` so server git badges/hunks/summary fire.
    let _ = Command::new("git")
        .args(["init"])
        .current_dir(&ws_dir)
        .output();
    let _ = Command::new("git")
        .args(["config", "user.email", "ci@lava.org"])
        .current_dir(&ws_dir)
        .output();
    let _ = Command::new("git")
        .args(["config", "user.name", "Lava CI"])
        .current_dir(&ws_dir)
        .output();
    let _ = Command::new("git")
        .args(["add", "z_tail/deep_needle_12049.rs"])
        .current_dir(&ws_dir)
        .output();
    let _ = Command::new("git")
        .args(["commit", "-m", "seed needle"])
        .current_dir(&ws_dir)
        .output();
    fs::write(
        tail_dir.join("deep_needle_12049.rs"),
        "// TODO: verify uncapped server ranking\npub fn needle_target() -> i32 { 12049 }\npub fn added_after_commit() -> i32 { 7 }\n",
    )
    .expect("modify needle");

    let orig_path = env::var("PATH").unwrap_or_default();
    let orig_install = env::var("LAVA_REMOTE_INSTALL_DIR").ok();
    // SAFETY: Guarded by `ENV_LOCK`.
    unsafe {
        env::set_var("PATH", format!("{}:{orig_path}", bin_dir.display()));
        env::set_var("LAVA_REMOTE_INSTALL_DIR", &install_dir);
    }

    let server_bin = find_lava_server_bin();
    let mgr = Arc::new(RemoteManager::new());
    let wake_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let wake_clone = Arc::clone(&wake_counter);
    mgr.set_waker(Arc::new(move || {
        wake_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));

    let deployed = mgr
        .deploy_server(
            "ssh://127.0.0.1",
            server_bin.to_str().expect("utf8 server_bin"),
        )
        .expect("deploy lava-server");
    assert_eq!(deployed.mode, "lava-server");

    let ws_uri = format!("ssh://127.0.0.1{}", ws_dir.display());
    let tail_uri = format!("ssh://127.0.0.1{}", tail_dir.display());
    let needle_uri = format!(
        "ssh://127.0.0.1{}",
        tail_dir.join("deep_needle_12049.rs").display()
    );
    let tall_uri = format!("ssh://127.0.0.1{}", big_file.display());

    // 1. Uncapped >12,000-file index + minimal render slice transfer (<= 150 items)
    let empty_rank = mgr
        .search_files_ranked(&ws_uri, "", false, false, 150)
        .expect("empty ranked search");
    assert!(
        empty_rank.total_indexed >= 12_052,
        "expected >= 12,052 indexed files on lava-server, got {}",
        empty_rank.total_indexed
    );
    assert_eq!(
        empty_rank.items.len(),
        150,
        "must transmit ONLY 150 renderable items over SSH, not all 12,052 paths"
    );

    let needle_rank = mgr
        .search_files_ranked(&ws_uri, "z_tail 12049", false, true, 150)
        .expect("Orderless ranked search past 12,000th file");
    assert_eq!(needle_rank.total_matched, 1);
    assert_eq!(needle_rank.items[0].rel_path, "z_tail/deep_needle_12049.rs");

    // 2. Windowed viewport preview slice around row 12,500 (transfers ~1 KiB instead of 480 KiB!)
    let pv = mgr
        .read_preview_window(&tall_uri, 12_494, 24, 8192)
        .expect("windowed preview at row 12,494");
    assert_eq!(pv.start_row, 12_494);
    assert_eq!(pv.lines.len(), 24);
    assert!(
        pv.lines[6].contains("DEEP_ROW_12500_MARKER"),
        "expected row 12,500 marker at window offset 6, got: {:?}",
        pv.lines[6]
    );

    // 3. Single-RTT `dir_snapshot` with server-joined git badge & preview
    let snap = mgr
        .dir_snapshot(
            &tail_uri,
            false,
            "",
            "name",
            false,
            "deep_needle_12049.rs",
            0,
            true,
            true,
        )
        .expect("1-RTT dir_snapshot");
    assert!(!snap.parent_entries.is_empty(), "parent entries included");
    assert_eq!(snap.entries.len(), 1);
    assert_eq!(
        snap.entries[0].git_badge, "M",
        "server joined modified git badge"
    );
    assert_eq!(snap.preview.kind, "text");
    assert!(
        snap.preview
            .lines
            .iter()
            .any(|l| l.contains("needle_target")),
        "focus preview included in same single RPC"
    );

    // 4. Minimal `git_summary`, `git_file_hunks`, `scan_todos`, and `workspace_replace`
    let gsum = mgr.git_summary(&ws_uri).expect("git_summary");
    assert!(gsum.is_repo && gsum.modified_count >= 1);

    let (tracked, hunks) = mgr
        .git_file_hunks(&tail_uri, &needle_uri, None)
        .expect("git_file_hunks");
    assert!(tracked && !hunks.is_empty());

    let todos = mgr.scan_todos(&tail_uri, 50).expect("scan_todos");
    assert_eq!(todos.len(), 1);
    assert_eq!(todos[0].3, "TODO");

    let (fc, tr) = mgr
        .workspace_replace(
            &tail_uri,
            "added_after_commit",
            "replaced_on_server",
            true,
            ".rs",
        )
        .expect("workspace_replace");
    assert_eq!((fc, tr), (1, 1));

    // 5. Non-blocking `submit_async` + latest-wins coalescing under 70ms simulated latency!
    set_simulated_rpc_delay_ms(70);
    let t0 = std::time::Instant::now();
    for q in ["mod_00001", "mod_00002", "mod_00003", "z_tail 12049"] {
        let _ = mgr.submit_async(
            "telescope.search".to_string(),
            AsyncRemoteOp::SearchFiles {
                uri: ws_uri.clone(),
                query: q.to_string(),
                include_hidden: false,
                include_dirs: false,
                limit: 50,
            },
        );
    }
    assert!(
        t0.elapsed().as_millis() < 20,
        "submit_async burst must never block caller thread!"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut final_hit = None;
    while std::time::Instant::now() < deadline {
        match mgr.poll_async("telescope.search") {
            AsyncPollOutcome::Ready {
                result: AsyncRemoteResult::SearchFiles(rf),
                ..
            } => {
                if rf.items.len() == 1 && rf.items[0].rel_path == "z_tail/deep_needle_12049.rs" {
                    final_hit = Some(rf);
                    break;
                }
            }
            AsyncPollOutcome::Error { message, .. } => panic!("async error: {message}"),
            _ => std::thread::sleep(std::time::Duration::from_millis(15)),
        }
    }
    set_simulated_rpc_delay_ms(0);
    assert!(
        final_hit.is_some(),
        "coalesced async slot must settle on final query 'z_tail 12049'"
    );
    assert!(
        wake_counter.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "completion must invoke registered UI waker"
    );

    let _ = mgr.disconnect("ssh://127.0.0.1");
    // SAFETY: Restore environment variables under `ENV_LOCK`.
    unsafe {
        env::set_var("PATH", orig_path);
        match orig_install {
            Some(v) => env::set_var("LAVA_REMOTE_INSTALL_DIR", v),
            None => env::remove_var("LAVA_REMOTE_INSTALL_DIR"),
        }
    }
    let _ = fs::remove_dir_all(&tmp_root);
}
