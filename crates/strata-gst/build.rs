fn main() {
    // Dynamically set build date so release metadata is always accurate.
    // Uses the UNIX `date` command to avoid adding a build dependency.
    let output = std::process::Command::new("date")
        .args(["+%Y-%m-%d"])
        .output();
    let date = match output {
        Ok(out) if out.status.success() => String::from_utf8(out.stdout)
            .unwrap_or_else(|_| "unknown".to_string())
            .trim()
            .to_string(),
        _ => "unknown".to_string(),
    };
    println!("cargo:rustc-env=BUILD_REL_DATE={}", date);
}
