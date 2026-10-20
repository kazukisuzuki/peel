# Peel

Optimizes JPEG and PNG images losslessly in place.

It removes metadata and trailing data. This includes EXIF and color profiles. Files are left unchanged if optimization fails. APNG is not supported


## Build

~~~sh
cargo build --release --locked
~~~


## Install

~~~sh
cargo install --force --locked --path .
~~~

~~~sh
cargo install --force --locked --git https://github.com/kazukisuzuki/peel.git
~~~


## Uninstall

~~~sh
cargo uninstall peel
~~~


## Usage

~~~sh
peel image.jpg image.png
peel "images/**/*.jpg" "images/**/*.jpeg" "images/**/*.png"
~~~

Quote glob patterns for consistent behavior across shells.


## License

Licensed under the [MIT License](LICENSE).

Third-party notices are in [thirdparty/README.md](thirdparty/README.md).
