// Makes PNG icons fully opaque, on white, with no transparency channel: Apple refuses iOS app
// icons that have one (App Store Connect checks the 1024-pixel icon). Uses only macOS's own
// CoreGraphics, so nothing to install (2026-10-27).
//
//   swift scripts/opaque_icons.swift crates/eudora-app/gen/apple/Assets.xcassets/AppIcon.appiconset/*.png
//
// Each file is replaced in place, at the same size.
import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

var failed = false
for path in CommandLine.arguments.dropFirst() {
    let url = URL(fileURLWithPath: path)
    guard let source = CGImageSourceCreateWithURL(url as CFURL, nil),
          let image = CGImageSourceCreateImageAtIndex(source, 0, nil) else {
        print("Couldn't read \(path)")
        failed = true
        continue
    }
    let (w, h) = (image.width, image.height)
    // RGB with no alpha (the fourth byte is skipped), painted white, the icon drawn over it
    guard let context = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0,
                                  space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                  bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue) else {
        print("Couldn't draw \(path)")
        failed = true
        continue
    }
    context.setFillColor(CGColor(red: 1, green: 1, blue: 1, alpha: 1))
    context.fill(CGRect(x: 0, y: 0, width: w, height: h))
    context.draw(image, in: CGRect(x: 0, y: 0, width: w, height: h))
    guard let opaque = context.makeImage(),
          let dest = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil) else {
        print("Couldn't write \(path)")
        failed = true
        continue
    }
    CGImageDestinationAddImage(dest, opaque, nil)
    if !CGImageDestinationFinalize(dest) {
        print("Couldn't write \(path)")
        failed = true
        continue
    }
    print("\(url.lastPathComponent): \(w) × \(h), opaque")
}
exit(failed ? 1 : 0)
