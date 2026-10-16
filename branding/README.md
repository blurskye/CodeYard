# Codeyard branding

**Name:** Codeyard. **Tagline:** Git hosting, written in Rust.

The mark is a mint dot, a commit in the yard, ringed by eight light rays on a deep violet tile.
Colors: violet `#2a1b5c`, ice `#f3f0ff`, mint `#5eead4`.

- [Vector master](../../src/webui/assets/logo.svg): 256×256 SVG, the source of the other files.
- [Browser raster](../../src/webui/assets/logo.webp): 512×512 lossless WebP of the master, used on the
  sign-in and home pages.
- [Favicon](../../src/webui/assets/favicon.png): 128×128 PNG of the master, used for the browser tab and
  the shell brand mark.
- [Banner source](banner.svg) and [banner PNG](banner.png): 1280×640 artwork for the README and the
  repository social preview. The PNG uses the Adwaita Sans typeface (SIL Open Font License).

## Re-export

ImageMagick is needed only when changing branding, not for normal app builds.

```sh
magick -background none src/webui/assets/logo.svg -resize 512x512 -define webp:lossless=true src/webui/assets/logo.webp
magick -background none src/webui/assets/logo.svg -resize 128x128 src/webui/assets/favicon.png
magick -size 1280x640 xc:'#f3f0ff' \( -background none src/webui/assets/logo.svg -resize 320x320 \) \
  -geometry +180+160 -composite -font Adwaita-Sans-Bold -pointsize 108 -fill '#2a1b5c' \
  -annotate +540+330 'Codeyard' -font Adwaita-Sans -pointsize 38 -fill '#5d5780' \
  -annotate +544+395 'Git hosting, written in Rust.' -strip branding/banner.png
```
