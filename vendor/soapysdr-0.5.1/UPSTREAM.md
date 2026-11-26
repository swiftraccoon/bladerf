# SoapySDR Rust wrapper source

The source comes from the published `soapysdr` 0.5.1 crate. The archive
SHA-256 is `a18cfe7033985b73ab920b34b79ea333ac9271c64910023054efefd8a5f08088`.
The included package provenance identifies upstream revision
`92211fe904f6683f57278cda0bd71344bc43fb9a`.

- `RxStream::read_with_metadata` exposes timestamp validity and terminal flags
  already returned by the same receive call.
- `RxStream::close` consumes an inactive receive stream and reports the status
  from `SoapySDRDevice_closeStream`. Its drop path skips the handle after an
  explicit native close attempt.

Remove the local changes when a qualified upstream version supplies equivalent
receive metadata and fallible close operations.
