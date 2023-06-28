# Rust Remote Shell

The current workspace is mainly divided into 2 parts:
* The `rust-remote-shell`, a library that facilitates the establishment of a WebSocket connection between a device and a host. This connection enables the host to issue shell commands to the device and subsequently await the output.
* The command-line interface (CLI), whose primary purpose of the is to assess and verify the functionality of the library.
