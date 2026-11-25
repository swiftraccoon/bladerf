# SoapySDR Rust wrapper source

The source comes from the published `soapysdr` 0.5.1 crate. The archive
SHA-256 is `a18cfe7033985b73ab920b34b79ea333ac9271c64910023054efefd8a5f08088`.
The included package provenance identifies upstream revision
`92211fe904f6683f57278cda0bd71344bc43fb9a`.

The local change adds `RxStream::read_with_metadata` to expose the timestamp
validity and terminal flags already returned by the same receive call. It uses
the existing stream ownership and receive implementation. Remove the local
patch when a qualified upstream version provides equivalent metadata access.
