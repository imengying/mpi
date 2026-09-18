fn main() {
    let cwd = std::env::temp_dir().join("mpi-rg-probe");
    std::fs::create_dir_all(&cwd).unwrap();
    println!("rg 存在？{:?}", mpi::auth::policy::trusted_executable("rg"));
    let d = mpi::auth::policy::assess_command("rg --pre 'evil' pattern", &cwd, mpi::auth::policy::Dialect::Zsh);
    println!("rg --pre → {:?}", d.reason().unwrap_or("(允许)"));
}
