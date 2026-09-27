# Layered (MV-HEVC) fixture

`mvhevc_960x960_3f.mov` — a three-frame stereo MV-HEVC QuickTime movie
(960×960, 8-bit 4:2:0, `hvc1` sample entry with `hvcC` + `lhvC`, view ids
0 / 1, both views in every access unit) produced by Apple VideoToolbox
through `AVAssetWriter` (`mvenc.swift`, black-box producer:
`swiftc -O -target arm64-apple-macos14.0 -o mvenc mvenc.swift && ./mvenc
out.mov 3`). The two views are synthetic gradients with a 40-pixel
horizontal shift between them.

The tests wrap access unit 0 as HEIF `lhv1` items (`hvcC` + `lhvC` +
`oinf` + `tols` 1, `lsel` 0 / 1, a `ster` group) and pin the decoded
views to the black-box decoder's per-view output (`ffmpeg -view_ids 0` /
`-view_ids 1`, rawvideo `yuv420p`, FNV-1a 64 of the planes).
