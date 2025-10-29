#![cfg(feature = "test")]

fn main() {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        println!("Still running...");
    }
}
