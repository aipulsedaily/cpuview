# Wallpaper credits

Every file in this folder may be redistributed with cpuview. Photos are public domain.
The `lofi-*` scenes were created for this project (see below). Files were checked one by one
on their Wikimedia Commons pages (license tag and author fields) on 2026-10-15.

| File | Title / subject | Author / creator | Source | License | Modifications |
|---|---|---|---|---|---|
| `nasa-webb-cosmic-cliffs.jpg` | "Cosmic Cliffs" in the Carina Nebula (NGC 3324), JWST NIRCam, 2022 | NASA, ESA, CSA, and STScI | [Commons](https://commons.wikimedia.org/wiki/File:NASA%E2%80%99s_Webb_Reveals_Cosmic_Cliffs,_Glittering_Landscape_of_Star_Birth.jpg) · [webbtelescope.org](https://webbtelescope.org/contents/media/images/2022/031/01G77PKB8NKR7S8Z6HBXMYATGJ) | Public domain (NASA; tagged PD on Commons). Credit line requested by STScI: *NASA, ESA, CSA, and STScI* | Downscaled from 14575×8441 to 1920×1112, re-encoded as JPEG q84, metadata stripped |
| `nasa-iss-aurora.jpg` | Aurora australis seen from the ISS (ISS023-E-58455), 2010 | ISS Expedition 23 crew, NASA | [Commons](https://commons.wikimedia.org/wiki/File:Aurora_Australis_From_ISS.JPG) | Public domain (NASA) | Downscaled from 4256×2832 to 1920×1278, JPEG q84, metadata stripped |
| `nasa-apollo8-earthrise.jpg` | Earthrise (AS8-14-2383), Apollo 8, 24 Dec 1968 | NASA / Bill Anders | [Commons](https://commons.wikimedia.org/wiki/File:NASA-Apollo8-Dec24-Earthrise.jpg) | Public domain (NASA) | 1920px rendition cropped to 16:9 (rows 780–1860) around the Earth and lunar horizon, JPEG q84 |
| `lofi-night-city.png` | Pixel-art night skyline with moon and lit windows | cpuview project (procedurally generated) | `tools/generate.py` | CC0 1.0 | — |
| `lofi-synthwave.jpg` | Striped sun over mountains and a neon grid | cpuview project (procedurally generated) | `tools/generate.py` | CC0 1.0 | — |
| `lofi-rainy-window.jpg` | Rain on a window, out-of-focus city lights | cpuview project (procedurally generated) | `tools/generate.py` | CC0 1.0 | — |
| `lofi-dusk-mountains.png` | Layered mountain ridges at dusk, pixel art | cpuview project (procedurally generated) | `tools/generate.py` | CC0 1.0 | — |
| `lofi-cozy-desk.png` | Desk with lamp, monitor, plant and a night window, pixel art | cpuview project (procedurally generated) | `tools/generate.py` | CC0 1.0 | — |

Licenses: public domain = no copyright in the US for works by NASA employees
([NASA media guidelines](https://www.nasa.gov/nasa-brand-center/images-and-media/)). NASA asks
that its imagery not be used to imply endorsement; cpuview doesn't. CC0 1.0:
<https://creativecommons.org/publicdomain/zero/1.0/>.

Regenerate the lofi scenes (deterministic, Pillow only):

```sh
python3 wallpapers/tools/generate.py wallpapers/
```

## Considered and left out

- *ESO – Milky Way* panorama (ESO/S. Brunier, CC BY 4.0): fine licence, but at half-block
  resolution the band shrinks to a thin grey line and the dashboard looks plain. The `space`
  theme covers that look procedurally.
- "Lofi girl" and similar branded or fan art, and anything from wallpaper aggregators,
  Unsplash or Pexels: their terms don't allow redistributing a collection.

## Credit shown in the app

The theme menu shows a one-line credit for the selected image, taken from this table
(`Author / creator · License`), e.g. `NASA / ISS Expedition 23 crew · public domain`. For
user-added images the credit line is empty.
