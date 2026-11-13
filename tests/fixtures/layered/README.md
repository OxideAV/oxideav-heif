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

# HEVC with an alpha layer

`hevc_alpha_128x96.mov` — a one-frame QuickTime movie whose HEVC access
unit carries two independent layers: `nuh_layer_id` 0 the 128×96 8-bit
4:2:0 picture, `nuh_layer_id` 1 its alpha plane (an auxiliary layer),
every parameter set in the sample entry's `hvcC`. Produced by Apple
VideoToolbox through a black-box encoder front end:

```
ffmpeg -f lavfi -i "testsrc2=s=128x96:r=1,format=bgra,geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='255*X/127'" \
  -frames:v 1 -c:v hevc_videotoolbox -alpha_quality 0.9 -q:v 60 -tag:v hvc1 hevc_alpha_128x96.mov
```

`tests/aux_layers.rs` wraps the access unit as an `hvc1` master item and
an `lhv1` alpha auxiliary item over the same bytes (`hvcC` with the
layer-0 parameter sets, `lhvC` with the layer-1 ones, `oinf`, `tols` 1,
`lsel` 1, `auxC` `urn:mpeg:hevc:2015:auxid:1`). The master is pinned to
the black-box decoder's picture (`ffmpeg -i … -f rawvideo -pix_fmt
yuv420p`, FNV-1a 64), the alpha plane to the source ramp
`255 · x / 127`.
