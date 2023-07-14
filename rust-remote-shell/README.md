# Rust Remote Shell Library

This library offers a seamless connection between an Astarte device and a host, enabling efficient communication via WebSocket connections. The primary components of this library are:
* **Astarte Device**: the Astarte device acts as a bridge between the host and a shell environment. It awaits Astarte events, establishing a robust WebSocket connection with the host. It efficiently handles incoming shell commands, executes them, and returns the corresponding output.
* **Host**: representing the client side, the host eagerly awaits a WebSocket connection from the device. It is capable of sending shell commands to the device and displaying responses upon successful connection.

Thanks to the integration of the [tower](https://docs.rs/tower/latest/tower/) crate, our library offers differnet services, each dedicated to specific connection tasks. From establishing a TCP connection between the host and device to handling individual shell commands, our library ensures a seamless and modularized experience for users.
