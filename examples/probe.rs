fn main() {
    let path = std::path::Path::new("/home/mengying/.local/share/mpi/sessions/01a0b35c-bbe4-70cf-b947-fcf321436e85.jsonl");
    let s = mpi::agent::session::Session::open(path).unwrap();
    println!("header.cwd = {}", s.header().cwd);
    println!("current_cwd = {:?}", s.current_cwd());
    let mut n = 0;
    for m in s.context_messages().iter().rev() {
        if mpi::agent::r#loop::is_environment_block(m) {
            println!("newest env block: {}", m.text().lines().find(|l| l.starts_with("工作目录")).unwrap_or("?"));
            n += 1;
            if n >= 3 { break; }
        }
    }
}
