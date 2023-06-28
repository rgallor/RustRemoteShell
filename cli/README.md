# CLI

The CLI is an easy way to use and test the `rust-remote-shell` library.

It defines 2 main commands, one to run a `Host` and the other to run a `Device`.

## Running a Host

To launch a host, it is necessary to specify a socket address (consisting of an IP address and port number) on which the host will listen for device connections.

The host can be executed over a plain TCP connection using the following command:
```sh
cargo run -- host <IP>::<PORT>
```

Alternatively, the host can be run with TLS to secure the WebSocket connection. In this case, the user needs to provide the host certificate and its private key. To simplify the process, [mkcert](https://github.com/FiloSottile/mkcert?search=1) or the [OpenSSL](https://www.openssl.org/docs/) toolkit can be used to generate PEM files containing this information.

To run the host over a TLS connection, use the following command:
```sh
cargo run -- host --tls-enabled --host-cert-file <CERT_PEM_FILE> --privkey-file <PKEY_PEM_FILE> <IP>::<PORT>
```

Similarly, the RUST_LOG flag can be utilized to set the desired log level and view logs during the application's execution. For example:
```sh
RUST_LOG=info cargo run -- host <IP>::<PORT>
```

The same principles apply to the device.

## Running a Device

The English in the provided text is generally correct. However, I have made some minor revisions for clarity and readability:

To launch a device, it is necessary to specify the path to a configuration file. The configuration file is a JSON file that contains useful information for establishing a connection between the device and [Astarte](https://docs.astarte-platform.org/astarte/latest/001-intro_user.html). It is important to note that this project is designed to work in conjunction with the Astarte platform, which means a direct WebSocket connection between a device and a host is not possible.

To run the device over a plain TCP connection, use the following command:
```sh
cargo run -- device <DEVICE_CONF.json>
```

Once the device is launched, it will await an Astarte event. The Astarte server will send data to the device based on a specific interface. In the `rust-remote-shell/interfaces/` folder, the interface specifies the connection scheme (ws or wss), the host IP, and the host port number. Only after receiving this configuration will the device attempt to connect with the host.

To properly configure Astarte, we recommend reading the [Astarte in 5 minutes guide](https://docs.astarte-platform.org/snapshot/010-astarte_in_5_minutes.html) and the [Astarte documentation](https://docs.astarte-platform.org/astarte/latest/001-intro_user.html). After setting up the Astarte environment, it is possible to send data to a device using the following command:
```sh
astartectl appengine --appengine-url http://localhost:4002/ --realm-management-url http://localhost:4000/ --realm-key <REALM>_private.pem --realm-name <REALM> devices send-data <DEVICE_ID> <INTERFACE> <ENDPOINT> <VALUE>
```
Please Note that we are using an Astarte object datastream, therefore the ENDPOINT corresponds to the common part of all the endopints in the interface mapping, and the VALUE, representing the three values to be sent, is organized in a JSON-like format.

Here is an example of a real command:
```sh
astartectl appengine --appengine-url http://localhost:4002/ --realm-management-url http://localhost:4000/ --realm-key test_private.pem --realm-name test devices send-data 2TBn-jNESuuHamE2Zo1anA org.astarte-platform.rust-remote-shell.ConnectToHost /rshell '{"scheme" : "ws", "host" : "127.0.0.1", "port" : 8080}'
```

Alternatively, the device can be run with TLS to secure the WebSocket connection. In this case, the user needs to provide the CA PEM certificate, which can also be generated using [mkcert](https://github.com/FiloSottile/mkcert?search=1) or [OpenSSL](https://www.openssl.org/docs/) toolkit.

To run the device over a TLS connection, use the following command:
```sh
cargo run -- device --tls-enabled --ca-cert-file <CERT_PEM_FILE> <DEVICE_CONF.json>
```

## Generate certificates and cryptographic keys

The easiest way to generate the required cryptographic materials is by using [mkcert](https://github.com/FiloSottile/mkcert?search=1). There are two main steps involved: creating a new local Certification Authority (CA) and generating a certificate for the host. To do this, follow these commands after installing `mkcert`:

1. Create a CA:
```sh
mkcert -install
```

2. Generate a certificate and the associated private key for the host:
```sh
mkcert <DOMAIN_NAME> [<OTHER_NAMES>]
```

The generated PEM files can be found in the folders specified by the program.

To access the CA.pem file, run the command `mkcert -CAROOT`. The host certificate and private keys will be created in the current directory where the `mkcert` command was executed.
