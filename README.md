# rsmake

A `make` implementation for GNU-free base systems.

rsmake implements a deliberately closed subset of the GNU make dialect:
a construct outside it is an error naming the construct, never a silent
skip. Conformance is proven differentially — the test suite runs every
corpus makefile through both real GNU `make` and rsmake and diffs the
output — while the implementation itself is white-room: written from the
GNU make manual, POSIX, and black-box observation only, never from GNU
source.

Zero dependencies, by policy.

## Building

```sh
cargo build --release
```

Running the full test suite requires GNU `make` on `PATH` (it is the
differential oracle):

```sh
cargo test
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms
or conditions.
