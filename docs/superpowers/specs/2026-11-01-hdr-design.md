# HDR-export av uppskalad video

Datum: 2026-11-01. Underlag: `docs/research/2026-11-01-hdr.md`.

## Varför

Bilden blir skarp och modern med AI-uppskalningen, men den är fortfarande SDR. Ägaren tittar
på en TV med HDR och vill att resultatet också ska utnyttja HDR: ljusare höglagen (lampor,
stjärnor, reflexer, explosioner) och bättre kontrast, utan att bilden byter karaktär.

## Ägarens beslut

1. **HDR är ett extra steg i kedjan** efter AI-uppskalningen. De uppskalade SDR-bildrutorna
   (`out/`) behålls och återanvänds; inga nya AI-bilder behövs för HDR.
2. **Formatet är HDR10+** som förval när HDR slås på. En HDR10+-fil innehåller alltid även
   HDR10:s statiska metadata, så samma fil spelas som HDR10 på TV-apparater utan HDR10+
   (LG, Sony). **HLG** finns som extra val. **Dolby Vision** byggs inte (licens).
3. **HDR-bildrutorna sparas** så att galleriet kan visa status per bildruta (SDR, HDR,
   konverteras, saknas) och så att HDR-bildrutor kan granskas och underkännas som SDR.
4. **HDR-bildrutorna tas bort efter lyckad export** som förval (ett helt avsnitt på 92 min
   skulle annars ta cirka 650 GB); valet kan stängas av.
5. Det som redan finns på datorn används: FFmpeg med `libplacebo` (inverse tone mapping på
   grafikkortet via Vulkan) och x265 10-bit. Inget nytt att installera.

## Mål

- Ett exportval: HDR *Av / HDR10+ / HDR10 / HLG* och toppljusstyrka *600 / 1000 / 1500 nits*
  (förval 1000).
- Bilden ser ut som originalet i mellantonerna; bara höglagen och kontrasten lyfts.
- Riktig metadata: MaxCLL/MaxFALL mätta på de faktiska HDR-bildrutorna; HDR10+ med
  ljusstatistik per scen.
- Att ändra HDR-val gör bara om HDR-steget och kodningen, aldrig AI-bilderna.
- Galleriet visar SDR/HDR-status, HDR-bildrutor tonmappade för datorskärmen, och de kan
  underkännas.

## Inte mål (nu)

- AI-baserad SDR→HDR (LumaFlux lokalt, Topaz Hyperion i molnet) – senare fas, efter ett
  snabbtest.
- Dolby Vision.
- HDR-visning i appen på en HDR-skärm (egui ritar SDR); HDR bedöms på TV:n.

## Får inte hända

- Bilden blir gråaktig, överexponerad, färgerna glider (fel matris/primärer) eller hud blir
  onaturlig.
- Flimmer i ljusstyrka mellan bildrutor eller vid scenbyten.
- Exporten kör i timmar och misslyckas först i slutet för att Vulkan saknas.
- Fel metadata (gissade MaxCLL/MaxFALL, fel enheter i HDR10+-JSON).
- H.264 med HDR (fungerar inte i praktiken): appen byter till H.265 och säger det.

## Hur det fungerar

1. **HDR-steg** efter uppskalningen: `libplacebo` gör om varje `out/`-bildruta från
   BT.709/sRGB till BT.2020 med PQ (HDR10/HDR10+) eller HLG, med `inverse_tonemapping`, och
   sparar 16-bitars PNG i `<jobb>/hdr/`. En nyckelfil i mappen (läge, topp, version) gör att
   ändrade val bara gör om HDR-steget.
2. **Mätning:** Rust läser HDR-bildrutorna, räknar om PQ till nits per kanal och tar fram
   per bildruta: max per kanal (MaxSCL), medel av maxRGB, och percentiler av maxRGB.
   MaxCLL = största maxRGB över alla bildrutor, MaxFALL = största medel-maxRGB.
3. **HDR10+:** scener från exportens scenbyteslista; JSON i det format x265 (`dhdr10-info`)
   och `hdr10plus_tool` använder; verifieras genom att `hdr10plus_tool extract` läser
   tillbaka metadatan ur den kodade filen.
4. **Kodning:** x265 10-bit (`yuv420p10le`), BT.2020 nc, PQ eller HLG, mastering display
   (P3-D65-primärer, luminans = vald topp), MaxCLL/MaxFALL uppmätta, `repeat-headers`.
   Fungerar ihop med "varannan/var tredje + interpolering" och anamorf utdata.
5. **Galleriet:** varje bildruta visar SDR och, när den finns, HDR-status; HDR-bildrutan
   visas tonmappad med en etikett. "Underkänn" på HDR-bilden konverterar om den; är felet i
   SDR-bilden gäller den vanliga underkännandet (ny AI-bild, sedan ny HDR).
6. **Vulkan kontrolleras innan exporten startar**; saknas det stoppar exporten direkt med en
   tydlig förklaring.

## Beslut tagna av Claude (kontrollera gärna)

- Mastering display beskrivs som P3-D65 i BT.2020-behållare (vanligast för HDR10-masters),
  luminans = vald topp, minsta 0,0001 nits.
- SDR-vitt placeras på 203 nits (ITU-R BT.2408) i expansionen.
- HDR-bildrutor sparas som 16-bitars PNG med högsta förlustfria komprimering.
- HLG får ingen mastering-metadata (standard för HLG).

## Lämnat åt byggaren

- Exakta `libplacebo`-parametrar för expansionen utöver ovanstående (efter ett visuellt test
  på testbildrutorna).
- Den tonmappning som används för att visa HDR-bildrutor i galleriet.
- Hur HDR-steget parallelliseras (det är snabbt; sekventiellt räcker troligen).

## Testa först

Fyra 16:9-bildrutor (bildruta 78043, planeten vid 276 s, bryggan vid 2700 s, loungen vid
840 s) som SDR och HDR10 i `tester\HDR\` – ägaren tittar på TV:n innan funktionen byggs ut.
