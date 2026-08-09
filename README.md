# OptiJPEG

Optimizes JPEG files losslessly in place.

It uses MozJPEG to preserve the quantized DCT coefficients and produce an optimized progressive JPEG.

All APP markers and comments are removed, including EXIF and ICC profiles.


## Build

~~~sh
cargo build --release
~~~


## Install

~~~sh
cargo install --force --locked --git https://github.com/kazukisuzuki/optijpeg.git
~~~


## Uninstall

~~~sh
cargo uninstall optijpeg
~~~


## Usage

~~~
optijpeg image1.jpg image2.jpg
optijpeg -r ./images1 ./images2
~~~

Only files ending in `.jpg` or `.jpeg`, case-insensitively, are processed.
Symbolic links are not followed. `-r/--recursive` accepts one or more
directories as search roots and cannot be combined with direct file arguments.
Files are processed in parallel, and results are printed in natural path order
(for example, `2.jpg` before `10.jpg`).

A file without APP or comment markers is left unchanged unless optimization
makes it smaller. A file containing those markers is replaced even when
removing them makes the result larger. Processing continues after errors, and
the exit status is non-zero if any file or recursive path fails.


## License

Licensed under the [MIT License](LICENSE).

Third-party license information and license texts are provided in [thirdparty/README.md](thirdparty/README.md).
