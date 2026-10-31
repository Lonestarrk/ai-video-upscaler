# AI Video Upscaler

En skrivbordsapp för Windows (och macOS) som skalar upp och restaurerar video med AI, i första hand med **ChatGPT via din vanliga ChatGPT-prenumeration**, så att du slipper betala per bild via API.

Dra in en video, hoppa i tidslinjen och jämför originalet med det uppskalade resultatet sida vid sida eller med en svepbar linje. När resultatet ser bra ut exporterar du hela videon eller ett urval. Ljudet kopieras oförändrat från originalet.

## Funktioner

- **Dra och släpp** en video (MP4, MOV, MKV, AVI, WebM …) eller öppna den via menyn.
- **Tidslinje** med miniatyrer, bildrutenoggrann stegning (⬅/➡, Shift för en sekund), hoppa till bildruta och **start/slut-markering** (I/O) för att exportera bara en del.
- **Jämförelse i två lägen:** *Sida vid sida* med en avdelare, eller *Svep* där du drar en linje över bilden. Zooma med scrollhjulet och panorera genom att dra. Båda halvorna följer med, så du jämför alltid samma detalj.
- **Förhandsvisning på begäran:** inget genereras förrän du klickar på *Skapa förhandsvisning* (P). Resultat sparas och visas direkt när du kommer tillbaka till samma bildruta. Gröna markeringar i tidslinjen visar vilka bildrutor som har en förhandsvisning.
- **Egen prompt** med sparade mallar. Appen lägger själv till tekniska regler: samma utsnitt, inga nya objekt, svarta kanter ska förbli svarta.
- **Konsekvens mellan bildrutor:** varje bildruta skickas tillsammans med den *föregående, redan uppskalade* bildrutan som referens, så att ansikten, texturer och färger hålls lika. Vid scenbyte (automatisk detektering) skickas ingen referens.
- **Bildformat-anpassning:** 16:9-bilder får svarta kanter till modellens 3:2-format, så att modellen inte beskär eller förvränger. Kanterna beskärs bort och resultatet skalas till exakt rätt storlek.
- **Sparar kvot:** identiska bildrutor återanvänder resultatet, och du kan välja att bara skicka varannan eller var tredje bildruta till AI:n och skapa resten med rörelseinterpolering.
- **Återupptagbar export:** färdiga bildrutor sparas. En export som avbryts, får nätverksfel eller når planens gräns fortsätter där den slutade när du startar den igen.
- **Utdata:** 2×/3×/4× eller 1080p/1440p/4K, H.264 eller H.265, valfri kvalitet (CRF).
- **Flera AI-tjänster:**
  | Tjänst | Kostnad | Hastighet | Kommentar |
  |---|---|---|---|
  | ChatGPT (prenumeration) | ingår i Plus/Pro/Business | ~1–2 min/bild | via Codex CLI, bäst kvalitet |
  | OpenAI API | per bild | snabbare, parallellt | gpt-image-2, API-nyckel |
  | Real-ESRGAN | gratis, lokalt | sekunder/bild | ingen prompt, kräver GPU med Vulkan |
  | Enkel uppskalning | gratis | direkt | för att testa flödet |
- **Svenska och engelska**, mörkt tema och justerbar storlek på gränssnittet.

## Kom igång

1. Installera appen (`AIVideoUpscaler-Setup-x.y.z.exe`) eller kör `AIVideoUpscaler.exe` direkt. Installationsprogrammet har FFmpeg med. Kör du bara exe-filen klickar du *Inställningar → FFmpeg och mappar → Hämta FFmpeg*.
2. **ChatGPT-prenumeration:** öppna *Inställningar → ChatGPT*.
   - Saknas Codex CLI klickar du *Installera Codex CLI*. Det kräver [Node.js](https://nodejs.org/), eller så installerar du det själv med `npm install -g @openai/codex`.
   - Klicka *Logga in med ChatGPT*. Webbläsaren öppnas. Välj **Sign in with ChatGPT** och logga in med ditt konto. Statusen blir grön.
3. Dra in en video, välj en bildruta och klicka *Skapa förhandsvisning*.
4. Justera prompten tills resultatet ser naturligt ut. Testexportera ett kort urval (I/O) och exportera sedan allt.

### Hur ChatGPT-prenumerationen används

OpenAI:s officiella [Codex CLI](https://github.com/openai/codex) loggar in med ditt ChatGPT-konto, och dess inbyggda bildgenerering (gpt-image-2) räknas mot planens gränser i stället för att debiteras per anrop. För varje bildruta kör appen `codex exec` i en tillfällig mapp. Bildrutan (och föregående resultat som referens) bifogas, och Codex instrueras att redigera bilden med bildverktyget och spara den som `output.png`. Om filen hamnar någon annanstans hämtas den från `~/.codex/generated_images`. Appen tar bort `OPENAI_API_KEY` från miljön för Codex, så att anropen inte av misstag faktureras via API.

Bra att veta:
- Gratisplanen har ingen bildgenerering i Codex. Plus räcker för förhandsvisningar och korta klipp. Planerna har gränser per tidsperiod, och bildgenerering förbrukar kvoten flera gånger snabbare än text.
- En sekund video är 24–30 bildrutor. Börja med korta urval, använd *varannan + interpolering* om det räcker, och låt långa exporter gå i omgångar. Jobbet fortsätter där det slutade.
- Codex kör med `workspace-write`-sandlåda och kan bara skriva i den tillfälliga mappen. Fungerar inte sandlådan på din dator kan du byta till `danger-full-access` under *Avancerat*.

## Bygga själv

Kräver Rust (stable). På Windows behövs MSVC-verktygen (Visual Studio "Desktop development with C++").

```powershell
cargo test --workspace
cargo build -p upscaler-gui --release
.\target\release\AIVideoUpscaler.exe [video.mp4]
```

Installationsprogram med [Inno Setup 6](https://jrsoftware.org/isinfo.php). Lägg gärna `ffmpeg.exe` och `ffprobe.exe` i `installer\ffmpeg\` först, så följer de med:

```powershell
& "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe" /DAppVersion=0.1.0 installer\upscaler.iss
```

GitHub Actions (`.github/workflows/build.yml`) bygger exe och installationsprogram (med FFmpeg) för Windows samt en macOS-binär vid varje push. Filerna hittar du under *Actions → senaste körningen → Artifacts*.

### Struktur

- `crates/upscaler-core`: allt utom gränssnittet: inställningar, FFmpeg (läsa, plocka ut bildrutor, koda med originalljud), AI-tjänsterna (`provider/`), bildgeometri (kanter, beskärning, scenbyten), prompter och exportjobbet.
- `crates/upscaler-gui`: egui-appen: jämförelsevy, tidslinje, sidopanel och inställningar.

Inställningarna sparas i `%APPDATA%\AI Video Upscaler\settings.json`. Förhandsvisningar, exportjobb och loggen sparas i `%LOCALAPPDATA%\AI Video Upscaler\`. Arbetsmappen kan flyttas i inställningarna.

## Licens

MIT, se [LICENSE](LICENSE). FFmpeg-bygget som följer med installationsprogrammet är GPL.
