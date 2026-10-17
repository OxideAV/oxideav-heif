// Black-box probe of Apple ImageIO (test oracle, compiled on demand by
// tests/common/mod.rs when `swiftc` is present): for each file, the
// image count, the pixel size and the orientation ImageIO reports for
// the first image, and whether it decodes. One line per file:
//   count=<n> width=<w> height=<h> orientation=<o> image=<w>x<h>|none
import Foundation
import ImageIO

for path in CommandLine.arguments.dropFirst() {
    let url = URL(fileURLWithPath: path)
    guard let src = CGImageSourceCreateWithURL(url as CFURL, nil) else {
        print("count=0")
        continue
    }
    let n = CGImageSourceGetCount(src)
    let props = CGImageSourceCopyPropertiesAtIndex(src, 0, nil) as? [String: Any] ?? [:]
    let o = (props["Orientation"] as? Int) ?? 1
    let w = (props["PixelWidth"] as? Int) ?? 0
    let h = (props["PixelHeight"] as? Int) ?? 0
    var image = "none"
    if let img = CGImageSourceCreateImageAtIndex(src, 0, nil) {
        image = "\(img.width)x\(img.height)"
    }
    print("count=\(n) width=\(w) height=\(h) orientation=\(o) image=\(image)")
}
