fn main() {
    // Re-run build script only if assets or the build script itself change
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=assets/chart.html");
    println!("cargo:rerun-if-changed=assets/echarts.min.js");
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(target_os = "windows")]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("FileDescription", "HMI Database to CSV Converter");
        res.set("ProductName", "HMI-Converter");
        res.set("OriginalFilename", "hmi-converter.exe");

        if let Err(e) = res.compile() {
            eprintln!("cargo:warning=Failed to compile Windows resources: {e}");
        }
    }
}
