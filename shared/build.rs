fn main() {
    capnpc::CompilerCommand::new()
        .file("schema/protocol.capnp")
        .run()
        .expect("capnp schema compilation failed");
}
