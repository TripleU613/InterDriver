use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

const SDK_VERSION: &str = "10.0.28000.0";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.as_slice() {
        [group, command] if group == "windows-export" && command == "build" => {
            let root = workspace_root()?;
            build_reference(&root)?;
            build_rust(&root)
        }
        [group, command, implementation] if group == "windows-export" && command == "build" => {
            let root = workspace_root()?;
            match implementation.as_str() {
                "reference" => build_reference(&root),
                "rust" => build_rust(&root),
                _ => Err("implementation must be reference or rust".into()),
            }
        }
        [group, command] if group == "windows-export" && command == "package" => {
            let root = workspace_root()?;
            package(&root, "rust")
        }
        [group, command, implementation] if group == "windows-export" && command == "package" => {
            let root = workspace_root()?;
            package(&root, implementation)
        }
        [group, command] if group == "windows-export" && command == "sign-test" => {
            sign_test(&env::current_dir().map_err(display_error)?)
        }
        [group, command, package_dir] if group == "windows-export" && command == "sign-test" => {
            sign_test(Path::new(package_dir))
        }
        [group, command] if group == "windows-export" && command == "install" => {
            install(&env::current_dir().map_err(display_error)?)
        }
        [group, command, package_dir] if group == "windows-export" && command == "install" => {
            install(Path::new(package_dir))
        }
        [group, command] if group == "windows-export" && command == "restore" => restore(),
        [group, command] if group == "windows-ude" && command == "build" => {
            build_ude(&workspace_root()?)
        }
        [group, command] if group == "windows-ude" && command == "package" => {
            package_ude(&workspace_root()?)
        }
        [group, command] if group == "wdk" && command == "fetch" => fetch_wdk(&workspace_root()?),
        [group, command] if group == "linux" && command == "probe" => probe_linux(),
        [group, command] if group == "linux" && command == "export" => {
            export_linux(&workspace_root()?, None)
        }
        [group, command, bus_id] if group == "linux" && command == "export" => {
            export_linux(&workspace_root()?, Some(bus_id))
        }
        [group, command, host] if group == "linux" && command == "attach" => {
            attach_linux(&workspace_root()?, host, None)
        }
        [group, command, host, bus_id] if group == "linux" && command == "attach" => {
            attach_linux(&workspace_root()?, host, Some(bus_id))
        }
        [group, command] if group == "macos" && command == "probe" => probe_macos(),
        [group, command] if group == "macos" && command == "enable-import" => {
            enable_macos_import(&workspace_root()?, None)
        }
        [group, command, binary] if group == "macos" && command == "enable-import" => {
            enable_macos_import(&workspace_root()?, Some(Path::new(binary)))
        }
        _ => {
            print_help();
            Err("invalid xtask command".into())
        }
    }
}

fn print_help() {
    println!("cargo xtask windows-export build [reference|rust]");
    println!("cargo xtask windows-export package [reference|rust]");
    println!("cargo xtask windows-export sign-test [package-directory]");
    println!("cargo xtask windows-export restore   (undo install; also runs at boot after a crash)");
    println!("cargo xtask windows-export install [package-directory]");
    println!("cargo xtask windows-ude build|package");
    println!("cargo xtask wdk fetch");
    println!("cargo xtask linux probe|export [bus-id]|attach <host> [bus-id]");
    println!("cargo xtask macos probe|enable-import [binary]");
}

fn workspace_root() -> Result<PathBuf, String> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "xtask has no workspace parent".into())
}

fn build_reference(root: &Path) -> Result<(), String> {
    let clang = find_tool(&["clang-cl", "clang-cl-21", "clang-cl-20", "clang-cl-19"])?;
    let linker = find_tool(&["lld-link", "lld-link-21", "lld-link-20", "lld-link-19"])?;
    let wdk = root.join(".wdk/microsoft.windows.wdk.x64.d/c");
    let sdk = root.join(".wdk/microsoft.windows.sdk.cpp.d/c");
    let sdk_lib = root.join(".wdk/microsoft.windows.sdk.cpp.x64.d/c");
    let output = root.join("driver/windows-export/build");
    fs::create_dir_all(&output).map_err(display_error)?;
    let source = root.join("driver/windows-export/driver.c");
    let object = output.join("driver.obj");
    let includes = [
        wdk.join(format!("Include/{SDK_VERSION}/km")),
        wdk.join(format!("Include/{SDK_VERSION}/km/crt")),
        wdk.join(format!("Include/{SDK_VERSION}/shared")),
        wdk.join("Include/wdf/kmdf/1.33"),
        sdk.join(format!("Include/{SDK_VERSION}/shared")),
        sdk.join(format!("Include/{SDK_VERSION}/um")),
        sdk.join(format!("Include/{SDK_VERSION}/ucrt")),
    ];
    let mut compile = Command::new(clang);
    compile.args([
        "--target=x86_64-pc-windows-msvc",
        "/c",
        "/GS-",
        "/W3",
        "/Od",
        "/Zi",
        "-D_AMD64_",
        "-DAMD64",
        "-D_WIN64",
        "-D_KERNEL_MODE",
        "-DNTDDI_VERSION=0x0A000010",
        "-D_WIN32_WINNT=0x0A00",
        "-DWINVER=0x0A00",
        "-DKMDF_VERSION_MAJOR=1",
        "-DKMDF_VERSION_MINOR=33",
        "-Wno-unused-command-line-argument",
        "-Wno-microsoft-anon-tag",
    ]);
    for include in includes {
        compile.arg(format!("-I{}", include.display()));
    }
    compile
        .arg(format!("/Fo{}", object.display()))
        .arg("--")
        .arg(source);
    run_command(&mut compile)?;

    let sys = output.join("interdriver-export.sys");
    let map = output.join("interdriver-export.map");
    let mut link = Command::new(linker);
    link.args([
        format!("/OUT:{}", sys.display()),
        "/DRIVER".into(),
        "/SUBSYSTEM:NATIVE,10.00".into(),
        "/OSVERSION:10.0".into(),
        "/ENTRY:FxDriverEntry".into(),
        "/NODEFAULTLIB".into(),
        "/MACHINE:X64".into(),
        "/DEBUG".into(),
        format!("/MAP:{}", map.display()),
        "/BASE:0x1D0000000".into(),
        "/DYNAMICBASE".into(),
        "/NXCOMPAT".into(),
        "/TSAWARE:NO".into(),
        "/RELEASE".into(),
        "/ALIGN:0x1000".into(),
        "/FILEALIGN:0x1000".into(),
        "/SECTION:.text,ERP".into(),
        "/SECTION:.rdata,RP".into(),
        "/SECTION:.data,RWP".into(),
        "/SECTION:.pdata,RP".into(),
        "/MERGE:_guard_check_icall_=.data".into(),
        "/MERGE:_guard_dispatch_icall_=.data".into(),
        "/MERGE:memset_=.data".into(),
        "/MERGE:.retplne=.data".into(),
        "/MERGE:.00cfg=.data".into(),
        format!(
            "/LIBPATH:{}",
            wdk.join(format!("Lib/{SDK_VERSION}/km/x64")).display()
        ),
        format!("/LIBPATH:{}", wdk.join("Lib/wdf/kmdf/x64/1.33").display()),
        format!("/LIBPATH:{}", sdk_lib.join("um/x64").display()),
        object.display().to_string(),
        "wdfdriverentry.lib".into(),
        "wdfldr.lib".into(),
        "ntoskrnl.lib".into(),
        "hal.lib".into(),
        "usbd.lib".into(),
        "bufferoverflowfastfailk.lib".into(),
    ]);
    run_command(&mut link)
}

fn build_ude(root: &Path) -> Result<(), String> {
    let clang = find_tool(&["clang-cl", "clang-cl-21", "clang-cl-20", "clang-cl-19"])?;
    let linker = find_tool(&["lld-link", "lld-link-21", "lld-link-20", "lld-link-19"])?;
    let wdk = root.join(".wdk/microsoft.windows.wdk.x64.d/c");
    let sdk = root.join(".wdk/microsoft.windows.sdk.cpp.d/c");
    let sdk_lib = root.join(".wdk/microsoft.windows.sdk.cpp.x64.d/c");
    let output = root.join("driver/windows-ude/build");
    fs::create_dir_all(&output).map_err(display_error)?;
    let object = output.join("driver.obj");
    let includes = [
        wdk.join(format!("Include/{SDK_VERSION}/km")),
        wdk.join(format!("Include/{SDK_VERSION}/km/crt")),
        wdk.join(format!("Include/{SDK_VERSION}/km/ude")),
        wdk.join(format!("Include/{SDK_VERSION}/shared")),
        wdk.join("Include/wdf/kmdf/1.33"),
        wdk.join("Include/wdf/kmdf/1.23"),
        sdk.join(format!("Include/{SDK_VERSION}/shared")),
        sdk.join(format!("Include/{SDK_VERSION}/um")),
        sdk.join(format!("Include/{SDK_VERSION}/ucrt")),
    ];
    let mut compile = Command::new(clang);
    compile.args([
        "--target=x86_64-pc-windows-msvc",
        "/c",
        "/GS-",
        "/W3",
        "/Od",
        "/Zi",
        "-D_AMD64_",
        "-DAMD64",
        "-D_WIN64",
        "-D_KERNEL_MODE",
        "-DNTDDI_VERSION=0x0A000010",
        "-D_WIN32_WINNT=0x0A00",
        "-DWINVER=0x0A00",
        "-DKMDF_VERSION_MAJOR=1",
        "-DKMDF_VERSION_MINOR=33",
        "-Wno-unused-command-line-argument",
        "-Wno-microsoft-anon-tag",
    ]);
    for include in includes {
        compile.arg(format!("-I{}", include.display()));
    }
    compile
        .arg(format!("/Fo{}", object.display()))
        .arg("--")
        .arg(root.join("driver/windows-ude/driver.c"));
    run_command(&mut compile)?;
    let mut link = Command::new(linker);
    link.args([
        format!("/OUT:{}", output.join("interdriver-ude.sys").display()),
        "/DRIVER".into(),
        "/SUBSYSTEM:NATIVE,10.00".into(),
        "/OSVERSION:10.0".into(),
        "/ENTRY:FxDriverEntry".into(),
        "/NODEFAULTLIB".into(),
        "/MACHINE:X64".into(),
        "/DEBUG".into(),
        format!("/MAP:{}", output.join("interdriver-ude.map").display()),
        "/BASE:0x1C0000000".into(),
        "/DYNAMICBASE".into(),
        "/NXCOMPAT".into(),
        "/TSAWARE:NO".into(),
        "/RELEASE".into(),
        "/ALIGN:0x1000".into(),
        "/FILEALIGN:0x1000".into(),
        "/SECTION:.text,ERP".into(),
        "/SECTION:.rdata,RP".into(),
        "/SECTION:.data,RWP".into(),
        "/SECTION:.pdata,RP".into(),
        "/MERGE:_guard_check_icall_=.data".into(),
        "/MERGE:_guard_dispatch_icall_=.data".into(),
        "/MERGE:memset_=.data".into(),
        "/MERGE:.retplne=.data".into(),
        "/MERGE:.00cfg=.data".into(),
        format!(
            "/LIBPATH:{}",
            wdk.join(format!("Lib/{SDK_VERSION}/km/x64")).display()
        ),
        format!(
            "/LIBPATH:{}",
            wdk.join(format!("Lib/{SDK_VERSION}/km/x64/ude/1.0"))
                .display()
        ),
        format!("/LIBPATH:{}", wdk.join("Lib/wdf/kmdf/x64/1.33").display()),
        format!("/LIBPATH:{}", sdk_lib.join("um/x64").display()),
        object.display().to_string(),
        "wdfdriverentry.lib".into(),
        "wdfldr.lib".into(),
        "udecxstub.lib".into(),
        "ntoskrnl.lib".into(),
        "hal.lib".into(),
        "bufferoverflowfastfailk.lib".into(),
    ]);
    run_command(&mut link)?;
    println!("built: {}", output.join("interdriver-ude.sys").display());
    Ok(())
}

fn package_ude(root: &Path) -> Result<(), String> {
    build_ude(root)?;
    let mut helper = Command::new("cargo");
    helper.current_dir(root).args([
        "build",
        "--release",
        "--target",
        "x86_64-pc-windows-gnu",
        "-p",
        "xtask",
    ]);
    run_command(&mut helper)?;
    let output = root.join("driver/windows-ude/package");
    fs::create_dir_all(&output).map_err(display_error)?;
    copy(
        &root.join("driver/windows-ude/build/interdriver-ude.sys"),
        &output.join("interdriver-ude.sys"),
    )?;
    copy(
        &root.join("driver/windows-ude/interdriver-ude.inf"),
        &output.join("interdriver-ude.inf"),
    )?;
    copy(
        &root.join("target/x86_64-pc-windows-gnu/release/xtask.exe"),
        &output.join("interdriver-driver-tool.exe"),
    )?;
    copy_packaging_tool(root, "signtool.exe", &output)?;
    copy_packaging_tool(root, "Inf2Cat.exe", &output)?;
    println!("staged UDE package at {}", output.display());
    Ok(())
}

fn build_rust(root: &Path) -> Result<(), String> {
    let wdk_root = merged_wdk_root(root)?;
    let linker = find_tool(&["lld-link", "lld-link-21", "lld-link-20", "lld-link-19"])?;
    let clang = find_tool(&["clang-cl", "clang-cl-21", "clang-cl-20", "clang-cl-19"])?;
    let mut cargo = Command::new("cargo");
    cargo
        .current_dir(root.join("driver/windows-export-rs"))
        .args(["build", "--release", "--target", "x86_64-pc-windows-msvc"])
        .env("WDKContentRoot", wdk_root)
        .env("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER", linker)
        .env("CC_x86_64_pc_windows_msvc", &clang)
        .env("CC_X86_64_PC_WINDOWS_MSVC", &clang)
        .env("RUSTFLAGS", "-C target-feature=+crt-static -C panic=abort");
    run_command(&mut cargo)?;
    let release = root.join("driver/windows-export-rs/target/x86_64-pc-windows-msvc/release");
    copy(
        &release.join("interdriver_export.dll"),
        &release.join("interdriver-export.sys"),
    )?;
    println!("built: {}", release.join("interdriver-export.sys").display());
    Ok(())
}

fn package(root: &Path, implementation: &str) -> Result<(), String> {
    match implementation {
        "reference" => build_reference(root)?,
        "rust" => build_rust(root)?,
        _ => return Err("implementation must be reference or rust".into()),
    }
    let mut cli = Command::new("cargo");
    cli.current_dir(root).args([
        "build",
        "--release",
        "--target",
        "x86_64-pc-windows-gnu",
        "-p",
        "interdriver-usb",
        "-p",
        "xtask",
    ]);
    run_command(&mut cli)?;
    let output = root
        .join("driver/windows-export/package")
        .join(implementation);
    fs::create_dir_all(&output).map_err(display_error)?;
    let sys = if implementation == "rust" {
        root.join(
            "driver/windows-export-rs/target/x86_64-pc-windows-msvc/release/interdriver-export.sys",
        )
    } else {
        root.join("driver/windows-export/build/interdriver-export.sys")
    };
    copy(&sys, &output.join("interdriver-export.sys"))?;
    let inf = if implementation == "rust" {
        root.join("driver/windows-export-rs/interdriver-export.inf")
    } else {
        root.join("driver/windows-export/interdriver-export.inf")
    };
    for (source, name) in [
        (inf, "interdriver-export.inf"),
        (
            root.join("target/x86_64-pc-windows-gnu/release/interdriver-usb.exe"),
            "interdriver-usb.exe",
        ),
        (
            root.join("target/x86_64-pc-windows-gnu/release/xtask.exe"),
            "interdriver-driver-tool.exe",
        ),
    ] {
        copy(&source, &output.join(name))?;
    }
    copy_packaging_tool(root, "signtool.exe", &output)?;
    copy_packaging_tool(root, "Inf2Cat.exe", &output)?;
    println!("staged {} package at {}", implementation, output.display());
    Ok(())
}

fn copy_packaging_tool(root: &Path, name: &str, output: &Path) -> Result<(), String> {
    let source = match name {
        "signtool.exe" => root.join(format!(
            ".wdk/microsoft.windows.sdk.cpp.d/c/bin/{SDK_VERSION}/x64/signtool.exe"
        )),
        "Inf2Cat.exe" => root.join(format!(
            ".wdk/microsoft.windows.wdk.x64.d/c/bin/{SDK_VERSION}/x86/Inf2Cat.exe"
        )),
        _ => return Err(format!("unknown packaging tool: {name}")),
    };
    copy(&source, &output.join(name))?;
    if name == "Inf2Cat.exe" {
        let directory = source
            .parent()
            .ok_or_else(|| "Inf2Cat has no parent directory".to_string())?;
        for dependency in [
            "Microsoft.UniversalStore.HardwareWorkflow.Cabinets.dll",
            "Microsoft.UniversalStore.HardwareWorkflow.Catalogs.dll",
            "Microsoft.UniversalStore.HardwareWorkflow.InfReader.dll",
            "Microsoft.UniversalStore.HardwareWorkflow.SubmissionBuilder.dll",
            "WindowsProtectedFiles.xml",
        ] {
            copy(&directory.join(dependency), &output.join(dependency))?;
        }
    }
    Ok(())
}

fn sign_test(package_dir: &Path) -> Result<(), String> {
    let package_dir = fs::canonicalize(package_dir).map_err(display_error)?;
    let sys = package_dir.join("interdriver-export.sys");
    let inf = package_dir.join("interdriver-export.inf");
    require_file(&sys)?;
    require_file(&inf)?;
    let signtool = package_tool(&package_dir, "signtool.exe");
    let inf2cat = package_tool(&package_dir, "Inf2Cat.exe");
    let certreq = system_tool("certreq.exe");
    let certutil = system_tool("certutil.exe");
    let certificate = package_dir.join("interdriver-test.cer");
    let request_config = package_dir.join(".interdriver-test-cert.inf");
    if !certificate.exists() {
        fs::write(
            &request_config,
            concat!(
                "[Version]\r\n",
                "Signature=\"$Windows NT$\"\r\n",
                "[NewRequest]\r\n",
                "Subject=\"CN=InterDriver Test Driver Signing\"\r\n",
                "KeyAlgorithm=RSA\r\n",
                "KeyLength=3072\r\n",
                "HashAlgorithm=sha256\r\n",
                "MachineKeySet=TRUE\r\n",
                "Exportable=TRUE\r\n",
                "RequestType=Cert\r\n",
                "ProviderName=\"Microsoft Software Key Storage Provider\"\r\n",
                "[EnhancedKeyUsageExtension]\r\n",
                "OID=1.3.6.1.5.5.7.3.3\r\n",
            ),
        )
        .map_err(display_error)?;
        let mut command = Command::new(&certreq);
        command
            .args(["-new"])
            .arg(&request_config)
            .arg(&certificate);
        run_command(&mut command)?;
        let _ = fs::remove_file(&request_config);
    }
    for store in ["My", "Root", "TrustedPublisher"] {
        let mut command = Command::new(&certutil);
        command.args(["-f", "-addstore", store]).arg(&certificate);
        run_command(&mut command)?;
    }
    sign_file(&signtool, &sys)?;
    let cat = package_dir.join("interdriver-export.cat");
    if cat.exists() {
        fs::remove_file(&cat).map_err(display_error)?;
    }
    // Keep deployment tools outside Inf2Cat's input. In particular, the
    // currently-running helper executable is locked by Windows and cannot be
    // copied by Inf2Cat's signability pass.
    let catalog_input = package_dir.join(".interdriver-catalog-input");
    fs::create_dir_all(&catalog_input).map_err(display_error)?;
    copy(&sys, &catalog_input.join("interdriver-export.sys"))?;
    copy(&inf, &catalog_input.join("interdriver-export.inf"))?;
    let generated_cat = catalog_input.join("interdriver-export.cat");
    if generated_cat.exists() {
        fs::remove_file(&generated_cat).map_err(display_error)?;
    }
    let mut catalog = Command::new(&inf2cat);
    catalog
        .arg(format!("/driver:{}", external_path(&catalog_input)))
        .arg("/os:10_X64");
    run_command(&mut catalog)?;
    copy(&generated_cat, &cat)?;
    sign_file(&signtool, &cat)?;
    for file in [&sys, &cat] {
        let mut verify = Command::new(&signtool);
        verify.args(["verify", "/pa", "/v"]).arg(file);
        run_command(&mut verify)?;
    }
    println!("test-signed package at {}", package_dir.display());
    Ok(())
}

fn sign_file(signtool: &Path, file: &Path) -> Result<(), String> {
    let mut command = Command::new(signtool);
    command
        .args([
            "sign",
            "/v",
            "/sm",
            "/s",
            "My",
            "/n",
            "InterDriver Test Driver Signing",
            "/a",
            "/fd",
            "SHA256",
        ])
        .arg(file);
    run_command(&mut command)
}

const RESTORE_TASK: &str = "InterDriverExportRestore";
const APPLE_SERVICES: [&str; 2] = ["Apple Mobile Device Service", "Bonjour Service"];
const APPLE_INTERFACES: [&str; 2] = [
    r"USB\VID_05AC&PID_12A8&MI_00",
    r"USB\VID_05AC&PID_12A8&MI_01",
];
const FIREWALL_RULE: &str = "InterDriver USB Exporter";

/// Every reversible change `install` made to the machine, persisted so a later
/// `restore` — including one run by the boot task after a crash or power loss —
/// can undo exactly what was done and no more. Kept as plain `key=value` lines
/// so it survives without a serialization dependency and can be read by eye.
#[derive(Debug, Default, PartialEq, Eq)]
struct RestoreManifest {
    services: Vec<String>,
    interfaces: Vec<String>,
    inf_published: Option<String>,
    firewall_rule: Option<String>,
}

impl RestoreManifest {
    fn to_text(&self) -> String {
        let mut out = String::from("# InterDriver export restore manifest\n");
        for service in &self.services {
            out.push_str(&format!("service={service}\n"));
        }
        for interface in &self.interfaces {
            out.push_str(&format!("interface={interface}\n"));
        }
        if let Some(inf) = &self.inf_published {
            out.push_str(&format!("inf={inf}\n"));
        }
        if let Some(rule) = &self.firewall_rule {
            out.push_str(&format!("firewall={rule}\n"));
        }
        out
    }

    fn parse(text: &str) -> Self {
        let mut manifest = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.to_owned();
            match key {
                "service" => manifest.services.push(value),
                "interface" => manifest.interfaces.push(value),
                "inf" => manifest.inf_published = Some(value),
                "firewall" => manifest.firewall_rule = Some(value),
                _ => {}
            }
        }
        manifest
    }
}

fn state_dir() -> PathBuf {
    PathBuf::from(env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into()))
        .join("InterDriver")
}

fn manifest_path() -> PathBuf {
    state_dir().join("export-state.txt")
}

/// Runs a command, returning its exit code and captured stdout.
fn run_capture(command: &mut Command) -> Result<(Option<i32>, String), String> {
    eprintln!("running {command:?}");
    let output = command.output().map_err(display_error)?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    print!("{text}");
    Ok((output.status.code(), text))
}

/// Parses the `Published Name: oemNN.inf` line pnputil prints so restore can
/// remove exactly the package this install added.
fn parse_published_inf(output: &str) -> Option<String> {
    for line in output.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(index) = lower.find("published name") {
            let rest = &line[index..];
            if let Some((_, value)) = rest.split_once(':') {
                let name = value.trim();
                if name.to_ascii_lowercase().ends_with(".inf") {
                    return Some(name.to_owned());
                }
            }
        }
    }
    None
}

fn install(package_dir: &Path) -> Result<(), String> {
    let package_dir = fs::canonicalize(package_dir).map_err(display_error)?;
    let inf = package_dir.join("interdriver-export.inf");
    let cli = package_dir.join("interdriver-usb.exe");
    require_file(&inf)?;
    require_file(&package_dir.join("interdriver-export.sys"))?;
    require_file(&package_dir.join("interdriver-export.cat"))?;
    require_file(&cli)?;
    let sc = system_tool("sc.exe");
    let taskkill = system_tool("taskkill.exe");
    let pnputil = system_tool("pnputil.exe");
    let netsh = system_tool("netsh.exe");

    // Record every change as it is made, and persist the manifest and a boot
    // task BEFORE the disruptive steps, so even a crash between here and the end
    // leaves the machine recoverable on the next boot.
    let mut manifest = RestoreManifest::default();
    if let Err(error) = write_manifest_and_arm_boot_task(&manifest) {
        eprintln!("warning: could not arm the restore safety net: {error}");
    }

    for service in APPLE_SERVICES {
        let mut command = Command::new(&sc);
        command.args(["stop", service]);
        run_best_effort(&mut command)?;
        manifest.services.push(service.to_owned());
    }
    for process in ["interdriver-usb.exe", "interdriver-usb-final.exe"] {
        let mut command = Command::new(&taskkill);
        command.args(["/f", "/im", process]);
        run_best_effort(&mut command)?;
    }
    for interface in APPLE_INTERFACES {
        let mut command = Command::new(&pnputil);
        command.args(["/disable-device", "/deviceid", interface, "/force"]);
        run_best_effort(&mut command)?;
        manifest.interfaces.push(interface.to_owned());
    }
    // Re-persist now that we know the services and interfaces we touched.
    let _ = write_manifest_and_arm_boot_task(&manifest);

    let mut add_driver = Command::new(&pnputil);
    add_driver.args(["/add-driver"]).arg(&inf);
    let (code, output) = run_capture(&mut add_driver)?;
    if code != Some(0) && code != Some(3010) {
        return Err(format!("pnputil failed with exit code {code:?}"));
    }
    manifest.inf_published = parse_published_inf(&output);
    let _ = write_manifest_and_arm_boot_task(&manifest);

    let mut bound = false;
    for mode in [
        r"USB\VID_05AC&PID_12A8",
        r"USB\VID_05AC&PID_1281",
        r"USB\VID_05AC&PID_1227",
    ] {
        let mut command = Command::new(&cli);
        command.args(["windows", "force-driver", mode]).arg(&inf);
        if run_status(&mut command)? == Some(0) {
            bound = true;
        }
    }
    if !bound {
        return Err("no attached iPhone parent accepted the driver".into());
    }
    let mut delete_rule = Command::new(&netsh);
    delete_rule.args([
        "advfirewall",
        "firewall",
        "delete",
        "rule",
        &format!("name={FIREWALL_RULE}"),
    ]);
    run_best_effort(&mut delete_rule)?;
    let mut add_rule = Command::new(&netsh);
    add_rule.args([
        "advfirewall",
        "firewall",
        "add",
        "rule",
        &format!("name={FIREWALL_RULE}"),
        "dir=in",
        "action=allow",
        "protocol=TCP",
        "localport=3240",
    ]);
    run_command(&mut add_rule)?;
    manifest.firewall_rule = Some(FIREWALL_RULE.to_owned());
    let _ = write_manifest_and_arm_boot_task(&manifest);

    let mut list = Command::new(&cli);
    list.args(["windows", "list"]);
    run_command(&mut list)?;
    println!("installed package from {}", package_dir.display());
    println!(
        "A restore manifest is at {} and a boot task will restore the machine\n\
         if this process is killed or the machine loses power. To restore now:\n\
         interdriver-driver-tool.exe windows-export restore",
        manifest_path().display()
    );
    println!("Apple and Bonjour services remain stopped while this PC exports the iPhone.");
    Ok(())
}

/// Persists the manifest and registers a boot-time restore task. The task runs
/// a stable copy of this tool, so restore works even if the package directory
/// is gone. Both are best-effort but logged; the manifest is the source of
/// truth and the task is the safety net for an ungraceful exit.
fn write_manifest_and_arm_boot_task(manifest: &RestoreManifest) -> Result<(), String> {
    let dir = state_dir();
    fs::create_dir_all(&dir).map_err(display_error)?;
    fs::write(manifest_path(), manifest.to_text()).map_err(display_error)?;

    // Copy this running tool to a stable location the boot task can invoke.
    let stable_tool = dir.join("interdriver-driver-tool.exe");
    if let Ok(current) = env::current_exe()
        && current != stable_tool
    {
        let _ = fs::copy(&current, &stable_tool);
    }
    let tool = if stable_tool.is_file() {
        stable_tool
    } else {
        env::current_exe().map_err(display_error)?
    };
    let schtasks = system_tool("schtasks.exe");
    let mut register = Command::new(&schtasks);
    register.args([
        "/Create",
        "/TN",
        RESTORE_TASK,
        "/TR",
        &format!("\"{}\" windows-export restore", tool.display()),
        "/SC",
        "ONSTART",
        "/RU",
        "SYSTEM",
        "/F",
    ]);
    let _ = run_status(&mut register);
    Ok(())
}

/// Reverses everything `install` recorded, in the opposite order, best-effort so
/// one failing step never blocks the rest. Idempotent: with no manifest it
/// reports that the machine is already stock and exits successfully, which is
/// what makes the boot task safe to leave armed and safe to run twice.
fn restore() -> Result<(), String> {
    let path = manifest_path();
    let Ok(text) = fs::read_to_string(&path) else {
        println!("no InterDriver export state found; machine is already stock");
        let _ = disarm_boot_task();
        return Ok(());
    };
    let manifest = RestoreManifest::parse(&text);
    let pnputil = system_tool("pnputil.exe");
    let netsh = system_tool("netsh.exe");
    let sc = system_tool("sc.exe");

    if let Some(rule) = &manifest.firewall_rule {
        let mut command = Command::new(&netsh);
        command.args([
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            &format!("name={rule}"),
        ]);
        let _ = run_status(&mut command);
    }
    // Removing our driver package unbinds every device forced onto it and lets
    // Windows fall back to the next-best driver, which is Apple's.
    if let Some(inf) = &manifest.inf_published {
        let mut command = Command::new(&pnputil);
        command.args(["/delete-driver", inf, "/uninstall", "/force"]);
        let _ = run_status(&mut command);
    }
    for interface in &manifest.interfaces {
        let mut command = Command::new(&pnputil);
        command.args(["/enable-device", "/deviceid", interface]);
        let _ = run_status(&mut command);
    }
    // Rescan so the Apple functions re-enumerate onto Apple's driver.
    let mut scan = Command::new(&pnputil);
    scan.args(["/scan-devices"]);
    let _ = run_status(&mut scan);
    for service in &manifest.services {
        let mut command = Command::new(&sc);
        command.args(["start", service]);
        let _ = run_status(&mut command);
    }

    // Only clear the state once the reversal has been attempted in full.
    let _ = fs::remove_file(&path);
    let _ = disarm_boot_task();
    println!("restored: Apple driver reclaimed, services started, firewall rule removed");
    Ok(())
}

fn disarm_boot_task() -> Result<(), String> {
    let schtasks = system_tool("schtasks.exe");
    let mut command = Command::new(&schtasks);
    command.args(["/Delete", "/TN", RESTORE_TASK, "/F"]);
    let _ = run_status(&mut command);
    Ok(())
}

fn fetch_wdk(root: &Path) -> Result<(), String> {
    let version = env::var("WDK_VERSION").unwrap_or_else(|_| "10.0.28000.2526".into());
    let destination = root.join(".wdk");
    fs::create_dir_all(&destination).map_err(display_error)?;
    for package in [
        "microsoft.windows.wdk.x64",
        "microsoft.windows.sdk.cpp",
        "microsoft.windows.sdk.cpp.x64",
    ] {
        let extracted = destination.join(format!("{package}.d"));
        if extracted.is_dir() {
            continue;
        }
        println!("downloading {package} {version}");
        let archive = destination.join(format!("{package}.zip"));
        let mut curl = Command::new("curl");
        curl.args(["-fL", "--max-time", "600", "-o"])
            .arg(&archive)
            .arg(format!(
                "https://api.nuget.org/v3-flatcontainer/{package}/{version}/{package}.{version}.nupkg"
            ));
        run_command(&mut curl)?;
        fs::create_dir_all(&extracted).map_err(display_error)?;
        let mut extract = Command::new("bsdtar");
        extract.current_dir(&extracted).arg("-xf").arg(&archive);
        run_command(&mut extract)?;
    }
    let mut links = 0usize;
    for include_root in [
        destination.join("microsoft.windows.wdk.x64.d/c/Include"),
        destination.join("microsoft.windows.sdk.cpp.d/c/Include"),
    ] {
        links += add_case_variant_headers(&include_root)?;
    }
    println!("case-variant symlinks: {links}");
    println!("WDK ready at {}", destination.display());
    Ok(())
}

#[cfg(unix)]
fn add_case_variant_headers(root: &Path) -> Result<usize, String> {
    use std::collections::HashMap;
    use std::os::unix::fs::symlink;

    let mut made = 0usize;
    for directory in walk_directories(root)? {
        let entries = fs::read_dir(&directory).map_err(display_error)?;
        let mut names = HashMap::new();
        let mut headers = Vec::new();
        for entry in entries {
            let entry = entry.map_err(display_error)?;
            if !entry.file_type().map_err(display_error)?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            names.insert(name.to_ascii_lowercase(), name.clone());
            if name.to_ascii_lowercase().ends_with(".h")
                || name.to_ascii_lowercase().ends_with(".hpp")
            {
                headers.push(entry.path());
            }
        }
        for header in headers {
            let contents = fs::read_to_string(&header).unwrap_or_default();
            for line in contents.lines() {
                let Some(include) = parse_include_name(line) else {
                    continue;
                };
                let requested = Path::new(include.replace('\\', "/").as_str())
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned());
                let Some(requested) = requested else {
                    continue;
                };
                if directory.join(&requested).exists() {
                    continue;
                }
                let Some(actual) = names.get(&requested.to_ascii_lowercase()) else {
                    continue;
                };
                if actual == &requested {
                    continue;
                }
                if symlink(actual, directory.join(&requested)).is_ok() {
                    made += 1;
                }
            }
        }
    }
    Ok(made)
}

#[cfg(not(unix))]
fn add_case_variant_headers(_root: &Path) -> Result<usize, String> {
    Ok(0)
}

#[cfg(unix)]
fn parse_include_name(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let rest = line.strip_prefix('#')?.trim_start();
    let rest = rest.strip_prefix("include")?.trim_start();
    if let Some(rest) = rest.strip_prefix('<') {
        rest.split_once('>').map(|(name, _)| name)
    } else if let Some(rest) = rest.strip_prefix('"') {
        rest.split_once('"').map(|(name, _)| name)
    } else {
        None
    }
}

#[cfg(unix)]
fn walk_directories(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut directories = vec![root.to_path_buf()];
    let mut index = 0usize;
    while index < directories.len() {
        let directory = directories[index].clone();
        index += 1;
        for entry in fs::read_dir(&directory).map_err(display_error)? {
            let entry = entry.map_err(display_error)?;
            if entry.file_type().map_err(display_error)?.is_dir() {
                directories.push(entry.path());
            }
        }
    }
    Ok(directories)
}

fn probe_linux() -> Result<(), String> {
    println!(
        "os={}",
        command_output("uname", &["-srmo"]).unwrap_or_else(|| "unknown".into())
    );
    for tool in ["usbip", "usbipd"] {
        println!(
            "{tool}={}",
            find_in_path(tool)
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "missing".into())
        );
    }
    for module in ["usbip_core", "usbip_host", "vhci_hcd"] {
        let state = if Path::new("/sys/module").join(module).is_dir() {
            "loaded"
        } else if command_succeeds("modinfo", &[module]) {
            "available"
        } else {
            "missing"
        };
        println!("module.{module}={state}");
    }
    if find_in_path("systemctl").is_some() {
        let state = command_output("systemctl", &["is-active", "usbmuxd"])
            .unwrap_or_else(|| "unknown".into());
        println!("service.usbmuxd={state}");
    }
    let devices = apple_usb_devices()?;
    if devices.is_empty() {
        println!("apple.devices=none");
    }
    for (bus_id, path) in devices {
        let product = read_trimmed(&path.join("idProduct")).unwrap_or_else(|| "unknown".into());
        let serial = read_trimmed(&path.join("serial")).unwrap_or_else(|| "unavailable".into());
        let driver = symlink_name(&path.join("driver")).unwrap_or_else(|| "none".into());
        println!("apple.{bus_id}=product:{product} serial:{serial} driver:{driver}");
        let prefix = format!("{bus_id}:");
        for entry in fs::read_dir("/sys/bus/usb/devices").map_err(display_error)? {
            let entry = entry.map_err(display_error)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&prefix) {
                let driver =
                    symlink_name(&entry.path().join("driver")).unwrap_or_else(|| "none".into());
                println!("apple.{bus_id}.interface.{name}={driver}");
            }
        }
    }
    if find_in_path("lsusb").is_some() {
        let mut lsusb = Command::new("lsusb");
        lsusb.args(["-d", "05ac:"]);
        run_best_effort(&mut lsusb)?;
    }
    Ok(())
}

fn export_linux(root: &Path, requested_bus_id: Option<&String>) -> Result<(), String> {
    let bus_id = requested_bus_id
        .cloned()
        .or_else(|| apple_usb_devices().ok()?.first().map(|(id, _)| id.clone()))
        .ok_or_else(|| "no Apple device attached".to_string())?;
    let cli = interdriver_cli(root)?;
    let port = env::var("PORT").unwrap_or_else(|_| "3240".into());
    let address = command_output("tailscale", &["ip", "-4"])
        .and_then(|output| output.lines().next().map(str::to_owned))
        .or_else(|| {
            command_output("hostname", &["-I"])
                .and_then(|value| value.split_whitespace().next().map(str::to_owned))
        })
        .unwrap_or_else(|| "127.0.0.1".into());
    println!("exporting {bus_id} on {address}:{port}");
    println!("import it with: interdriver-usb probe {address} {bus_id}");
    let interrupted = Arc::new(AtomicBool::new(false));
    install_interrupt_handler(&interrupted)?;
    let mut child = Command::new(cli)
        .args(["linux", "export", &bus_id, &port])
        .spawn()
        .map_err(display_error)?;
    let result = loop {
        if let Some(status) = child.try_wait().map_err(display_error)? {
            break if status.success() {
                Ok(())
            } else {
                Err(format!("exporter exited with {status}"))
            };
        }
        if interrupted.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            break Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    };
    restore_linux_usb(&bus_id);
    result
}

fn attach_linux(root: &Path, host: &str, requested_bus_id: Option<&String>) -> Result<(), String> {
    let cli = interdriver_cli(root)?;
    let bus_id = match requested_bus_id {
        Some(bus_id) => bus_id.clone(),
        None => {
            let output = Command::new(&cli)
                .args(["linux", "remote-list", host])
                .output()
                .map_err(display_error)?;
            if !output.status.success() {
                return Err("remote-list failed".into());
            }
            String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .next()
                .map(str::to_owned)
                .ok_or_else(|| format!("no devices exported by {host}"))?
        }
    };
    let mut modprobe = Command::new("modprobe");
    modprobe.arg("vhci_hcd");
    run_command(&mut modprobe)?;
    let mut attach = Command::new("usbip");
    attach.args(["attach", "-r", host, "-b", &bus_id]);
    run_command(&mut attach)?;
    let ports = command_output("usbip", &["port"]).unwrap_or_default();
    let port = ports
        .lines()
        .filter_map(|line| line.trim().strip_prefix("Port "))
        .filter_map(|value| value.split_once(':').map(|(number, _)| number.to_owned()))
        .next_back()
        .ok_or_else(|| "usbip did not report the attached port".to_string())?;
    println!("attached {bus_id} from {host} on vhci port {port}");
    println!("press Ctrl-C to detach");
    let interrupted = Arc::new(AtomicBool::new(false));
    install_interrupt_handler(&interrupted)?;
    while !interrupted.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(250));
    }
    let mut detach = Command::new("usbip");
    detach.args(["detach", "-p", &port]);
    run_best_effort(&mut detach)?;
    restart_usbmuxd();
    Ok(())
}

fn install_interrupt_handler(interrupted: &Arc<AtomicBool>) -> Result<(), String> {
    let interrupted = Arc::clone(interrupted);
    ctrlc::set_handler(move || interrupted.store(true, Ordering::SeqCst)).map_err(display_error)
}

fn restore_linux_usb(bus_id: &str) {
    println!("restoring {bus_id}");
    let _ = fs::write("/sys/bus/usb/drivers/usb/unbind", bus_id);
    thread::sleep(Duration::from_secs(1));
    let _ = fs::write("/sys/bus/usb/drivers/usb/bind", bus_id);
    restart_usbmuxd();
}

fn restart_usbmuxd() {
    let mut command = Command::new("systemctl");
    command.args(["restart", "usbmuxd"]);
    let _ = run_best_effort(&mut command);
}

fn apple_usb_devices() -> Result<Vec<(String, PathBuf)>, String> {
    let mut result = Vec::new();
    for entry in fs::read_dir("/sys/bus/usb/devices").map_err(display_error)? {
        let entry = entry.map_err(display_error)?;
        if read_trimmed(&entry.path().join("idVendor")).as_deref() == Some("05ac") {
            result.push((
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            ));
        }
    }
    result.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(result)
}

fn interdriver_cli(root: &Path) -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("BIN") {
        let path = PathBuf::from(path);
        return require_file(&path).map(|_| path);
    }
    for path in [
        root.join("target/release/interdriver-usb"),
        root.join("target/debug/interdriver-usb"),
    ] {
        if path.is_file() {
            return Ok(path);
        }
    }
    Err("build first: cargo build --release -p interdriver-usb".into())
}

fn probe_macos() -> Result<(), String> {
    if command_output("uname", &["-s"]).as_deref() != Some("Darwin") {
        return Err("this probe must run on macOS".into());
    }
    print_command_output("sw_vers", &[]);
    println!(
        "arch={}",
        command_output("uname", &["-m"]).unwrap_or_else(|| "unknown".into())
    );
    println!(
        "sip={}",
        command_output("csrutil", &["status"]).unwrap_or_else(|| "unknown".into())
    );
    if command_succeeds("xcodebuild", &["-version"]) {
        print_command_output("xcodebuild", &["-version"]);
    } else {
        println!("xcode=missing");
    }
    let configurator = Path::new("/Applications/Apple Configurator.app");
    let cfgutil = configurator.join("Contents/MacOS/cfgutil");
    if configurator.is_dir() {
        let version = command_output(
            "mdls",
            &[
                "-raw",
                "-name",
                "kMDItemVersion",
                configurator.to_string_lossy().as_ref(),
            ],
        )
        .unwrap_or_else(|| "unknown".into());
        println!("configurator.version={version}");
    } else {
        println!("configurator=missing");
    }
    if cfgutil.is_file() {
        print_command_output(cfgutil.to_string_lossy().as_ref(), &["version"]);
        println!("cfgutil.devices.begin");
        print_command_output(cfgutil.to_string_lossy().as_ref(), &["list"]);
        println!("cfgutil.devices.end");
    } else {
        println!("cfgutil=missing");
    }
    println!(
        "iousbhost.framework={}",
        if Path::new("/System/Library/Frameworks/IOUSBHost.framework").is_dir() {
            "present"
        } else {
            "missing"
        }
    );
    println!("apple.usb.begin");
    if let Some(output) = command_output("system_profiler", &["SPUSBDataType"]) {
        let lines: Vec<&str> = output.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let lower = line.to_ascii_lowercase();
            if lower.contains("iphone") || lower.contains("ipad") || lower.contains("apple mobile")
            {
                for detail in lines.iter().skip(index).take(9) {
                    println!("{detail}");
                }
            }
        }
    }
    println!("apple.usb.end");
    if let Some(binary) = env::var_os("CLIENT_BIN") {
        println!("client.entitlements.begin");
        let mut command = Command::new("codesign");
        command.args(["-d", "--entitlements", ":-"]).arg(binary);
        run_best_effort(&mut command)?;
        println!("client.entitlements.end");
    }
    Ok(())
}

fn enable_macos_import(root: &Path, requested_binary: Option<&Path>) -> Result<(), String> {
    let binary = requested_binary
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.join("target/debug/interdriver-macos-controller-probe"));
    require_file(&binary)?;
    let entitlements = root.join("crates/platform-macos/entitlements.plist");
    require_file(&entitlements)?;
    println!("== machine security ==");
    print_command_output("csrutil", &["status"]);
    if find_in_path("bputil").is_some() {
        let mut command = Command::new("sudo");
        command.args(["bputil", "-d"]);
        run_best_effort(&mut command)?;
    }
    println!("\n== signing with the entitlement ==");
    let mut sign = Command::new("codesign");
    sign.args(["-s", "-", "--entitlements"])
        .arg(&entitlements)
        .arg("--force")
        .arg(&binary);
    run_command(&mut sign)?;
    let mut inspect = Command::new("codesign");
    inspect.args(["-d", "--entitlements", "-"]).arg(&binary);
    run_best_effort(&mut inspect)?;
    println!("\n== running ==");
    let status = Command::new(&binary).status().map_err(display_error)?;
    let code = exit_code_with_signal(&status);
    match code {
        0 => println!("RESULT: controller created — macOS import is unblocked on this machine"),
        137 => println!("RESULT: killed by AMFI (SIGKILL). SIP is still enforcing; see below."),
        other => println!("RESULT: exited {other} (see message above)"),
    }
    if code != 0 {
        println!(
            "\nTo unblock on THIS Mac (development only, never the release runner):\n\
             1. Shut down. Hold the power button until Loading startup options.\n\
             2. Options -> Continue, then Utilities -> Terminal: csrutil disable\n\
             3. Reboot, then rerun this command.\n\n\
             Shipping requires Apple's host-controller entitlement; see docs/apple-entitlement-request.md."
        );
    }
    Ok(())
}

#[cfg(unix)]
fn exit_code_with_signal(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

#[cfg(not(unix))]
fn exit_code_with_signal(status: &std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 && candidate.is_file() {
        return Some(candidate.to_path_buf());
    }
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

fn command_succeeds(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .status()
        .is_ok_and(|status| status.success())
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if text.is_empty() {
        text = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    }
    (!text.is_empty()).then_some(text)
}

fn print_command_output(program: &str, args: &[&str]) {
    if let Some(output) = command_output(program, args) {
        println!("{output}");
    }
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
}

fn symlink_name(path: &Path) -> Option<String> {
    fs::read_link(path).ok().and_then(|target| {
        target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
    })
}

fn require_file(path: &Path) -> Result<(), String> {
    if path.is_file() {
        Ok(())
    } else {
        Err(format!("required file is missing: {}", path.display()))
    }
}

fn package_tool(package_dir: &Path, name: &str) -> PathBuf {
    let local = package_dir.join(name);
    if local.is_file() {
        local
    } else {
        PathBuf::from(name)
    }
}

fn system_tool(name: &str) -> PathBuf {
    env::var_os("WINDIR")
        .map(PathBuf::from)
        .map(|path| path.join("System32").join(name))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

fn external_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    path.strip_prefix(r"\\?\").unwrap_or(&path).to_string()
}

fn merged_wdk_root(root: &Path) -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("WDKContentRoot") {
        return Ok(PathBuf::from(path));
    }
    let output = root.join(".wdk/rust-driver-root");
    let wdk = root.join(".wdk/microsoft.windows.wdk.x64.d/c");
    let sdk = root.join(".wdk/microsoft.windows.sdk.cpp.d/c");
    let sdk_lib = root.join(".wdk/microsoft.windows.sdk.cpp.x64.d/c");
    link_dir(&wdk.join("Include/wdf"), &output.join("Include/wdf"))?;
    link_dir(
        &wdk.join(format!("Include/{SDK_VERSION}/km")),
        &output.join(format!("Include/{SDK_VERSION}/km")),
    )?;
    for name in ["shared", "um", "ucrt"] {
        link_dir(
            &sdk.join(format!("Include/{SDK_VERSION}/{name}")),
            &output.join(format!("Include/{SDK_VERSION}/{name}")),
        )?;
    }
    link_dir(&wdk.join("Lib/wdf"), &output.join("Lib/wdf"))?;
    link_dir(
        &wdk.join(format!("Lib/{SDK_VERSION}/km")),
        &output.join(format!("Lib/{SDK_VERSION}/km")),
    )?;
    for name in ["um", "ucrt"] {
        link_dir(
            &sdk_lib.join(name),
            &output.join(format!("Lib/{SDK_VERSION}/{name}")),
        )?;
    }
    Ok(output)
}

#[cfg(unix)]
fn link_dir(source: &Path, destination: &Path) -> Result<(), String> {
    if destination.exists() {
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(display_error)?;
    }
    std::os::unix::fs::symlink(source, destination).map_err(display_error)
}

#[cfg(windows)]
fn link_dir(_source: &Path, _destination: &Path) -> Result<(), String> {
    Err("set WDKContentRoot to the installed Windows Kits root".into())
}

fn find_tool(names: &[&str]) -> Result<PathBuf, String> {
    for name in names {
        let status = Command::new(name).arg("--version").output();
        if status.is_ok_and(|output| output.status.success()) {
            return Ok(PathBuf::from(name));
        }
    }
    Err(format!(
        "none of these tools were found: {}",
        names.join(", ")
    ))
}

fn run_command(command: &mut Command) -> Result<(), String> {
    eprintln!("running {command:?}");
    let status = command.status().map_err(display_error)?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("command exited with {status}"))
    }
}

fn run_status(command: &mut Command) -> Result<Option<i32>, String> {
    eprintln!("running {command:?}");
    command
        .status()
        .map(|status| status.code())
        .map_err(display_error)
}

fn run_best_effort(command: &mut Command) -> Result<(), String> {
    run_status(command).map(|_| ())
}

fn copy(source: &Path, destination: &Path) -> Result<(), String> {
    fs::copy(source, destination).map(|_| ()).map_err(|error| {
        format!(
            "copy {} to {}: {error}",
            source.display(),
            destination.display()
        )
    })
}

fn display_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trips_through_text() {
        let manifest = RestoreManifest {
            services: vec![
                "Apple Mobile Device Service".into(),
                "Bonjour Service".into(),
            ],
            interfaces: vec![
                r"USB\VID_05AC&PID_12A8&MI_00".into(),
                r"USB\VID_05AC&PID_12A8&MI_01".into(),
            ],
            inf_published: Some("oem42.inf".into()),
            firewall_rule: Some("InterDriver USB Exporter".into()),
        };
        let parsed = RestoreManifest::parse(&manifest.to_text());
        assert_eq!(parsed, manifest);
    }

    #[test]
    fn manifest_ignores_comments_and_blank_lines() {
        let text = "# header\n\nservice=Bonjour Service\ninf=oem7.inf\n";
        let parsed = RestoreManifest::parse(text);
        assert_eq!(parsed.services, vec!["Bonjour Service".to_string()]);
        assert_eq!(parsed.inf_published.as_deref(), Some("oem7.inf"));
        assert!(parsed.interfaces.is_empty());
        assert!(parsed.firewall_rule.is_none());
    }

    #[test]
    fn parses_pnputil_published_name() {
        let output = "Adding driver package...\r\nDriver package added successfully.\r\nPublished Name:  oem23.inf\r\n";
        assert_eq!(parse_published_inf(output).as_deref(), Some("oem23.inf"));
    }

    #[test]
    fn published_name_absent_is_none() {
        assert_eq!(parse_published_inf("no useful output here"), None);
    }
}
