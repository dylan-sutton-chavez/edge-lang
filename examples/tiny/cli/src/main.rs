use cli::net::Net;
use std::sync::Arc;
use tiny_cli::kit;

fn main() {
    kit(Arc::new(Net)).main()
}
