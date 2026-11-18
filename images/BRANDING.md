# Violet / Copper vector branding

Drive, Stream, and Gallery share frontal violet–pink faces, shallow metallic copper backing, and narrow edge highlights. The icons are editable vector artwork with no embedded images or raster effects.

| App | Mark |
| --- | --- |
| Proton Drive for Linux | Open folder with lilac documents |
| Proton Stream | Rounded play prism |
| Proton Gallery | Stacked photo plates, mountains, and a copper sun |

## Master artwork

Edit `images/logo.svg`. It uses a 256-unit grid with transparent margins, a `mark` group, and reusable linear gradients. The copper backing extends six units up and right. SVG paths, rounded rectangles, circles, strokes, translation, and the photo clip are retained when exporting Android VectorDrawables. Unsupported SVG features cause generation to fail rather than silently change the artwork.

Earlier artwork is preserved in `images/branding-archive/`. The new assets replace the active branding without removing previous designs.

## Regeneration

```sh
python scripts/render-branding.py
python scripts/render-branding.py --check
```

Requires Python 3, librsvg (`rsvg-convert`), and ImageMagick (`magick`). Stream's TV banner also requires fontconfig and DejaVu Sans. Keep the generator identical across the three repositories; every checkout renders independently and requires no sibling checkout. Edit the canonical master rather than generated platform assets.

## Platform treatment

- Desktop SVGs and PNGs retain transparent margins and the exact approved geometry. The same colored artwork works on light and dark surfaces.
- Android adaptive icons use a pale lavender full-bleed background and a separate foreground containing the complete object. The 256-unit canvas maps to the central 64dp of the 108dp adaptive canvas; the painted geometry stays within the central safe circle. Android supplies the outer mask.
- Themed launcher icons and Stream's status-bar notification icon use a white silhouette without colored bevel highlights.
- iOS uses the same play prism over an opaque pale lavender square, exported at 1024px. The system supplies the corner mask.
- The Android TV banner pairs the play prism with the full Proton Stream name.

Keep full product names in launcher metadata. Application UI typography and photo-content icons are independent of this branding.

## Outputs

`packaging/io.narl.proton-drive-linux.svg` is installed by the desktop packages and embedded in the GTK resource bundle for in-app branding.

The Drive cover (`images/cover.svg` and `images/cover.png`) also embeds the canonical icon. The generator updates its `app-logo` group while preserving the surrounding cover layout.
