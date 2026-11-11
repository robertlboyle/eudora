# Third-party notices

Eudora is © Planet Networks, licensed MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`).
It includes or uses the following works under their own licenses.

## Classic Eudora (QUALCOMM)

Classic Eudora 7.1 for Windows is used as a reference, and parts of it are ported to Rust: its
filters (rules) and their file format, its menus, shortcuts, and options, and its mailbox and
table-of-contents formats. The program icon is redrawn from Eudora 6.2.1's, and Classic mode's
icons and the tray icon from Eudora 7.1's. The source was released by the Computer History
Museum under this license:

```
Copyright (c) 1996-2006 by QUALCOMM, Incorporated
Copyright (c) 2016, Computer History Museum
All rights reserved.
Redistribution and use in source and binary forms, with or without modification, are permitted
(subject to the limitations in the disclaimer below) provided that the following conditions are
met:
 * Redistributions of source code must retain the above copyright notice, this list of
   conditions and the following disclaimer.
 * Redistributions in binary form must reproduce the above copyright notice, this list of
   conditions and the following disclaimer in the documentation and/or other materials provided
   with the distribution.
 * Neither the name of Computer History Museum nor the names of its contributors may be used to
   endorse or promote products derived from this software without specific prior written
   permission.
NO EXPRESS OR IMPLIED LICENSES TO ANY PARTY'S PATENT RIGHTS ARE GRANTED BY THIS LICENSE. THIS
SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED
WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS
FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE
LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
(INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF
THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

## Font Awesome Free

Icons from Font Awesome Free (the fallback when Font Awesome Pro isn't available), © Fonticons,
Inc., licensed CC BY 4.0 (https://fontawesome.com/license/free). Official builds use Font Awesome
Pro under Planet Networks' license.

## ocrs text recognition models

Where the system has no OCR of its own, Eudora reads pictures with ocrs
(https://github.com/robertknight/ocrs, code MIT OR Apache-2.0). Its two models,
`text-detection.onnx` and `text-recognition.onnx`, © Robert Knight, are licensed
**CC BY-SA 4.0** (https://creativecommons.org/licenses/by-sa/4.0/) and were trained on the
HierText dataset (CC BY-SA 4.0). They aren't shipped with Eudora: they're downloaded, unchanged,
from https://ocrs-models.s3-accelerate.amazonaws.com/ the first time they're needed.

## Rust libraries

Eudora is built from Rust libraries (listed in `Cargo.lock`), each under a permissive license
(MIT, Apache-2.0, Apache-2.0 WITH LLVM-exception, BSD, ISC, Zlib, Unlicense or MPL-2.0); `cargo metadata` lists each one's license. Among
them: Tantivy, SQLite (public domain, through rusqlite), Tauri, rustls and ring, lopdf, ocrs, rten,
image, wasmtime and Cranelift (the Bytecode Alliance; Apache-2.0 WITH LLVM-exception: the plugins' sandbox), and Microsoft's `windows` crate.
