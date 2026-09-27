// Black-box MV-HEVC producer: Apple VideoToolbox through AVAssetWriter,
// two synthetic views (left / right differ), a few frames, 960x960.
import AVFoundation
import CoreMedia
import CoreVideo
import Foundation
import VideoToolbox

let args = CommandLine.arguments
let url = URL(fileURLWithPath: args[1])
let frames = args.count > 2 ? Int(args[2])! : 3
let w = 960, h = 960
try? FileManager.default.removeItem(at: url)
let writer = try! AVAssetWriter(outputURL: url, fileType: .mov)
let settings: [String: Any] = [
    AVVideoCodecKey: AVVideoCodecType.hevc,
    AVVideoWidthKey: w,
    AVVideoHeightKey: h,
    AVVideoCompressionPropertiesKey: [
        kVTCompressionPropertyKey_MVHEVCVideoLayerIDs as String: [0, 1],
        kVTCompressionPropertyKey_MVHEVCViewIDs as String: [0, 1],
        kVTCompressionPropertyKey_MVHEVCLeftAndRightViewIDs as String: [0, 1],
        kVTCompressionPropertyKey_HasLeftStereoEyeView as String: true,
        kVTCompressionPropertyKey_HasRightStereoEyeView as String: true,
        kVTCompressionPropertyKey_AverageBitRate as String: 6_000_000,
    ],
]
let input = AVAssetWriterInput(mediaType: .video, outputSettings: settings)
input.expectsMediaDataInRealTime = false
let attrs: [String: Any] = [
    kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    kCVPixelBufferWidthKey as String: w,
    kCVPixelBufferHeightKey as String: h,
]
let adaptor = AVAssetWriterInputTaggedPixelBufferGroupAdaptor(
    assetWriterInput: input, sourcePixelBufferAttributes: attrs)
writer.add(input)
writer.startWriting()
writer.startSession(atSourceTime: .zero)

func fill(_ pb: CVPixelBuffer, frame: Int, view: Int) {
    CVPixelBufferLockBaseAddress(pb, [])
    let yBase = CVPixelBufferGetBaseAddressOfPlane(pb, 0)!.assumingMemoryBound(to: UInt8.self)
    let yStride = CVPixelBufferGetBytesPerRowOfPlane(pb, 0)
    let cBase = CVPixelBufferGetBaseAddressOfPlane(pb, 1)!.assumingMemoryBound(to: UInt8.self)
    let cStride = CVPixelBufferGetBytesPerRowOfPlane(pb, 1)
    for y in 0..<h {
        for x in 0..<w {
            // A gradient with a view-dependent horizontal shift (a fake
            // disparity) and a frame-dependent drift.
            let v = (x + view * 40 + frame * 8) / 4 + (y / 6)
            yBase[y * yStride + x] = UInt8(16 + (v % 220))
        }
    }
    for y in 0..<(h / 2) {
        for x in 0..<(w / 2) {
            cBase[y * cStride + 2 * x] = UInt8(128 + ((x / 8 + view * 30) % 100) - 50)
            cBase[y * cStride + 2 * x + 1] = UInt8(128 + ((y / 8 + frame * 5) % 100) - 50)
        }
    }
    CVPixelBufferUnlockBaseAddress(pb, [])
}

for i in 0..<frames {
    while !input.isReadyForMoreMediaData { usleep(2000) }
    var left: CVPixelBuffer?
    var right: CVPixelBuffer?
    CVPixelBufferPoolCreatePixelBuffer(nil, adaptor.pixelBufferPool!, &left)
    CVPixelBufferPoolCreatePixelBuffer(nil, adaptor.pixelBufferPool!, &right)
    fill(left!, frame: i, view: 0)
    fill(right!, frame: i, view: 1)
    let lb = CMTaggedBuffer(tags: [.videoLayerID(0), .stereoView(.leftEye)], buffer: .pixelBuffer(left!))
    let rb = CMTaggedBuffer(tags: [.videoLayerID(1), .stereoView(.rightEye)], buffer: .pixelBuffer(right!))
    if !adaptor.appendTaggedBuffers([lb, rb], withPresentationTime: CMTime(value: CMTimeValue(i), timescale: 30)) {
        print("append failed: \(String(describing: writer.error))")
        exit(1)
    }
}
input.markAsFinished()
let sem = DispatchSemaphore(value: 0)
writer.finishWriting { sem.signal() }
sem.wait()
if writer.status != .completed {
    print("writer failed: \(String(describing: writer.error))")
    exit(1)
}
print("ok")
