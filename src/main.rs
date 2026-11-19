fn main() {
    if let Err(err) = gitserve::work() {
        eprintln!("Error: {err}");
    }
}
