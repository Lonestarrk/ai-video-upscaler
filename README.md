# AI Video Upscaler

En skrivbordsapp för Windows (och macOS) som skalar upp och restaurerar video med AI, i första hand med **ChatGPT via din vanliga ChatGPT-prenumeration**, så att du slipper betala per bild via API.

Dra in en video, hoppa i tidslinjen och jämför originalet med det uppskalade resultatet sida vid sida eller med en svepbar linje. När resultatet ser bra ut exporterar du hela videon eller ett urval. Ljudet kopieras oförändrat från originalet, eller görs om till 5.1 och får bättre kvalitet om du vill.

- [Funktioner](#funktioner)
- [Kom igång: använda appen](#kom-igång-använda-appen)
- [Ljud](#ljud)
- [Kom igång: bygga, testa och utveckla vidare](#kom-igång-bygga-testa-och-utveckla-vidare)
- [Felsökning](#felsökning)
- [Hur det fungerar](#hur-det-fungerar)

## Funktioner

- **Dra och släpp** en video (MP4, MOV, MKV, AVI, WebM, VOB, MPG, M2TS …) eller öppna den via menyn.
- **Tidslinje** med miniatyrer, bildrutenoggrann stegning (⬅/➡, Shift för en sekund), hoppa till bildruta och **start/slut-markering** (I/O) för att exportera bara en del.
- **Jämförelse i två lägen:** *Sida vid sida* med en avdelare, eller *Svep* där du drar en linje över bilden. Zooma med scrollhjulet och panorera genom att dra. Båda halvorna följer med, så du jämför alltid samma detalj. Originalet visas i sitt eget format och resultatet i det valda formatet.
- **Förhandsvisning på begäran:** inget genereras förrän du klickar på *Skapa förhandsvisning* (P). Resultat sparas och visas direkt när du kommer tillbaka till samma bildruta. Gröna markeringar i tidslinjen visar vilka bildrutor som har en förhandsvisning.
- **Egen prompt** med sparade mallar. Standardmallen *Trogen restaurering* restaurerar i ett enda anrop: struktur före detalj, inga påhittade detaljer (skuggor tolkas inte som anatomi), och identitet och ansiktsgeometri går före skärpa. Mallen *Enkel uppstädning* är en kortare, allmän variant. Appen lägger själv till tekniska regler: samma utsnitt, inga nya objekt, och föregående bildruta som referens.
- **Konsekvens mellan bildrutor:** varje bildruta skickas tillsammans med den *föregående, redan uppskalade* bildrutan som referens, så att ansikten, texturer och färger hålls lika. Vid scenbyte (automatisk detektering) skickas ingen referens.
- **DVD och gamla källor:**
  - Anamorfa pixlar (till exempel 720×480 i 16:9) sträcks till rätt form för AI:n och kläms tillbaka i exporten.
  - Interlaced video avflätas.
  - Svarta kanter (letterbox) hittas och tas bort automatiskt.
  - Felflaggat bildformat upptäcks och kan rättas. Det går också att ange källans format för hand.
- **Ändra bildformat:** till exempel 4:3 till 16:9. AI:n fyller ut det som saknas genom att fortsätta scenen.
- **Bildformat-anpassning:** modeller som bara svarar i vissa format får bilden med svarta kanter, som beskärs bort efteråt, så att inget förvrängs.
- **Sparar kvot:** identiska bildrutor återanvänder resultatet, och du kan välja att bara skicka varannan eller var tredje bildruta till AI:n och skapa resten med rörelseinterpolering.
- **Återupptagbar export:** färdiga bildrutor sparas. En export som avbryts, får nätverksfel eller når planens gräns fortsätter där den slutade när du startar den igen.
- **Utdata:** 2×/3×/4× eller 1080p/1440p/4K, H.264 eller H.265, valfri kvalitet (CRF).
- **Ljud:** originalljudet oförändrat, eller 5.1 gjort av stereoljudet (lokal AI eller klassisk uppmixning) med valbar basnivå, och förbättrad ljudkvalitet. Se [Ljud](#ljud).
- **Flera AI-tjänster:**

  | Tjänst | Kostnad | Hastighet | Föregående bildruta som referens | Kommentar |
  |---|---|---|---|---|
  | ChatGPT (prenumeration) | ingår i Plus/Pro/Business | ~1–2 min/bild | ja | via Codex CLI, ingen API-nyckel |
  | OpenAI API | per bild | snabbare, parallellt | ja | gpt-image-2 |
  | Google Gemini (Nano Banana Pro) | ~0,13 USD/bild i 2K | ~20–30 s/bild | ja | bland de bästa på att behålla identitet |
  | FLUX (Black Forest Labs) | per bild | ~10–20 s/bild | FLUX.2: ja, Kontext: nej | behåller bildens form |
  | Grok (xAI) | ~0,02 USD/bild | snabb | nej | billig, färre omdömen för trogen restaurering |
  | Real-ESRGAN | gratis, lokalt | sekunder/bild | – | ingen prompt, kräver GPU med Vulkan |
  | Enkel uppskalning | gratis | direkt | – | ingen AI, för att testa flödet |
- **Svenska och engelska**, mörkt tema och justerbar storlek på gränssnittet.

## Kom igång: använda appen

### 1. Hämta appen

Välj ett av sätten:

- **Färdigt bygge från GitHub:** öppna *Actions*, välj den senaste lyckade körningen av *Build* och ladda ner **AIVideoUpscaler-windows** under *Artifacts*. I zip-filen finns:
  - `AIVideoUpscaler-Setup-x.y.z.exe`, installationsprogrammet. **Varför:** det har FFmpeg med och lägger appen i Start-menyn.
  - `AIVideoUpscaler.exe`, som kan köras direkt utan installation.
- **Bygg själv:** se [nedan](#kom-igång-bygga-testa-och-utveckla-vidare).

Programmet är inte kodsignerat ännu. Därför kan Windows SmartScreen varna första gången: klicka *Mer information → Kör ändå*.

### 2. FFmpeg (krävs)

**Varför:** FFmpeg läser videon, plockar ut bildrutor, mäter svarta kanter och sätter ihop resultatet med originalljudet. Utan FFmpeg kan appen inte öppna någon video.

- Har du installerat med installationsprogrammet är FFmpeg redan med.
- Annars: *Inställningar → FFmpeg och mappar → Hämta FFmpeg* (cirka 100 MB, hamnar i `%LOCALAPPDATA%\AI Video Upscaler\tools`).
- Du kan också använda en egen FFmpeg i PATH (`winget install Gyan.FFmpeg`) eller ange mappen i inställningarna.

### 3. ChatGPT med din prenumeration (rekommenderat)

Du behöver tre saker:

| Vad | Varför |
|---|---|
| **ChatGPT Plus, Pro eller Business** | Bildgenerering via Codex ingår bara i betalda planer. Gratisplanen räcker inte. |
| **Node.js (LTS)** | Codex CLI är ett Node.js-program och installeras med `npm`, som följer med Node.js. Installera med `winget install OpenJS.NodeJS.LTS` eller från [nodejs.org](https://nodejs.org/). Starta om appen efteråt så att den hittar `npm`. |
| **Codex CLI** (OpenAI) | Det enda officiella sättet att använda en ChatGPT-prenumeration från ett program. Appen ber Codex skala upp varje bildruta, och kostnaden räknas mot planen i stället för per anrop. Installera med knappen *Inställningar → ChatGPT → Installera Codex CLI*, eller själv med `npm install -g @openai/codex`. |

Sedan:
1. *Inställningar → ChatGPT → Logga in med ChatGPT.* Webbläsaren öppnas. Välj **Sign in with ChatGPT**. **Varför:** Codex sparar inloggningen på din dator, och den kopplar anropen till din plan och inte till ett API-konto.
2. Statusen i sidopanelen ska bli grön: "Inloggad med ChatGPT".

Bra att veta:
- Planerna har en gräns för antal bilder per tidsperiod, och bildgenerering förbrukar kvoten flera gånger snabbare än text.
- En sekund video är 24–30 bildrutor. Börja med korta urval (I/O), använd *varannan + interpolering* om kvaliteten räcker, och låt långa exporter gå i omgångar. Jobbet fortsätter där det slutade.

### 4. Andra AI-tjänster (valfritt)

Alla andra tjänster debiteras per bild och kräver en egen API-nyckel. **Varför:** de har inget prenumerationsupplägg för program; nyckeln talar om vilket konto som betalar. Ange nyckeln under respektive flik i *Inställningar*. Lämnas fältet tomt används miljövariabeln i tabellen.

| Tjänst | Var du skapar nyckeln | Miljövariabel |
|---|---|---|
| OpenAI API | [platform.openai.com/api-keys](https://platform.openai.com/api-keys) | `OPENAI_API_KEY` |
| Google Gemini | [aistudio.google.com/apikey](https://aistudio.google.com/apikey) (betalning måste vara aktiverad; Google One/AI Pro täcker inte API:t) | `GEMINI_API_KEY` |
| FLUX | [dashboard.bfl.ai](https://dashboard.bfl.ai/) | `BFL_API_KEY` |
| Grok | [console.x.ai](https://console.x.ai/) | `XAI_API_KEY` |
| Real-ESRGAN | ingen nyckel: ladda ner [realesrgan-ncnn-vulkan](https://github.com/xinntao/Real-ESRGAN/releases) och peka ut programfilen | – |

Nycklarna sparas i inställningsfilen på din dator (`%APPDATA%\AI Video Upscaler\settings.json`).

### 5. Första körningen

1. Dra in en video och klicka på en punkt i tidslinjen.
2. Klicka *Skapa förhandsvisning* (eller P). Med ChatGPT tar det en till två minuter.
3. Jämför i *Sida vid sida* eller *Svep* och zooma in på ansikten.
4. Justera prompten och förhandsvisa igen. Vill du testa hela flödet utan kostnad först, välj tjänsten *Enkel uppskalning (test)*.
5. Markera ett kort urval med *Sätt start* och *Sätt slut* och exportera. Exportera sedan allt.

## Ljud

Som standard kopieras ljudet oförändrat. Under *Ljud* i sidopanelen kan du i stället göra 5.1 av ett stereospår och förbättra ljudkvaliteten. Ljudet görs för exakt samma bildrutor som exporteras och ligger i synk med bilden.

### 5.1 av stereo

**Varför:** de flesta äldre serier och filmer på DVD och i AVI har bara stereo (ofta Dolby Surround-matrisat). 5.1 lägger dialogen i mittkanalen, så att den hörs tydligt, och sprider musik och effekter runt om.

| Motor | Vad den gör | Tid |
|---|---|---|
| **AI (lokal, grafikkort)** | En AI-modell (BandIt Plus) delar upp ljudet i tal, musik och effekter. Talet hamnar i mitten (FC), musik och effekter i fram- och bakkanalerna, och basen från effekterna i LFE. Inget försvinner: det modellen inte kan placera ligger kvar fram. | ~25 s per minut ljud på grafikkortet, ~4 min per minut på processorn |
| **Klassisk (utan AI)** | Appen delar upp ljudet i många små frekvensband och flyttar det som ligger mitt i stereobilden (oftast dialogen) till mitten och tar bort det från fram-höger/vänster; skillnaden mellan kanalerna går fördröjt bak. Fungerar på alla datorer, men skiljer inte tal från musik som ligger i mitten. | några sekunder per minut |

Källan är redan 5.1: dess egna kanaler (och LFE) behålls, ingen uppmixning. Mono kan inte bli 5.1.

**Bas (LFE):** subwooferkanalen byggs av basen under 90 Hz och öppnar bara för bas som stiger över scenens jämna bakgrund (explosioner, nedslag, toppar i musiken). Ett jämnt motorbrus, som skeppets brum på bryggan, ger ingen LFE. Dialog öppnar den aldrig. Huvudkanalerna behåller all sin bas, så inget går förlorat om förstärkaren skickar basen till subwoofern ändå.

| Nivå | Vad den gör |
|---|---|
| Subtil | 3 dB svagare LFE än Normal, för små rum och grannar. |
| Normal | Standard. |
| Kraftig | 3 dB starkare LFE än Normal. |
| Biokänsla | Kraftig plus en oktav djupare bas (ner mot 35 Hz) som byggs av effektljudet. Bara med AI-motorn, som tar fram effekterna; med Klassisk blir den som Kraftig. |

### Förbättra ljudkvaliteten

**Varför:** AC-3, MP3 och AAC tar bort diskanten över en viss frekvens (ofta 14–16 kHz) och kan låta matta. AI-modellen Apollo återskapar bara det som saknas ovanför den gränsen; allt under gränsen är originalet, orört. Det går att använda med 5.1 (fram, mitt och bak var för sig, aldrig LFE) eller ensamt, då blir resultatet stereo. Tar ungefär 20 s per minut stereoljud på grafikkortet, längre med 5.1. Hur mycket det hörs beror på källan: på en AC-3-källa med lite diskant kvar är skillnaden liten.

### Ljud-AI: installera

AI-motorn och kvalitetsförbättringen behöver *Ljud-AI*: *Inställningar → Ljud-AI → Installera*. Appen hämtar Python, PyTorch och modellerna och lägger dem i `%LOCALAPPDATA%\AI Video Upscaler\tools\audio-ai`.

- **Storlek:** ~2 GB att ladda ner, **~5,7 GB** på disken med PyTorch för AMD-grafikkort (ROCm). Installationen tar runt 10 minuter och fortsätter där den slutade om den avbryts.
- **Grafikkort:** AMD Radeon med ROCm-stöd i Windows (testat på RX 7900 XTX, drivrutin 26.2.2 eller nyare). Hittas inget sådant kort installeras PyTorch för processorn i stället.
- **Processorn** fungerar men är långsam: ett 45-minutersavsnitt tar ~15 min på grafikkortet och ~3 timmar på processorn. Appen varnar innan.
- Ljud-AI finns bara för Windows. Klassisk 5.1 fungerar överallt.

### Den exporterade filen

Med ändrat ljud blir exporten en **MKV** med:

| Spår | Varför |
|---|---|
| **5.1 FLAC** (standardspår) | Förlustfritt, exakt det appen räknade fram. |
| **5.1 AC-3** 640 kbit/s | För förstärkare och TV-apparater som inte kan spela FLAC. Gjort av samma ljud som FLAC-spåret. |
| **Original** | Källans ljud, oförändrat, för att jämföra eller gå tillbaka. |

Med bara kvalitetsförbättring: FLAC stereo + Original.

**Varför MKV:** MP4 kan inte bära FLAC tillsammans med AC-3 och originalspåret på ett sätt som alla spelare klarar, och MKV hanterar flera ljudspår med namn och ett standardspår. Välj spår i spelaren (VLC, Kodi, Plex, mediaspelare i TV:n). Skulle förstärkaren vara tyst på FLAC-spåret, välj AC-3.

## Kom igång: bygga, testa och utveckla vidare

Det här behövs bara om du ska bygga appen själv eller fortsätta utvecklingen, till exempel med Claude Code.

### Verktyg

| Verktyg | Varför | Installera (Windows) |
|---|---|---|
| **Git** | Hämta koden och spara ändringar till GitHub. | `winget install Git.Git` |
| **Rust** (stable, via rustup) | Appen är skriven i Rust. rustup installerar kompilatorn och `cargo`, som bygger och testar. | `winget install Rustlang.Rustup` |
| **Visual Studio Build Tools** med "Desktop development with C++" | Rust på Windows använder Microsofts länkare och Windows SDK för att skapa exe-filen. Utan dem misslyckas bygget med `link.exe not found`. | `winget install Microsoft.VisualStudio.2022.BuildTools --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"` |
| **FFmpeg** i PATH | Testerna och testvideoskriptet anropar `ffmpeg` och `ffprobe` direkt. Utan dem hoppas videotesterna över. | `winget install Gyan.FFmpeg` |
| **Node.js** och **Codex CLI** | För att testa ChatGPT-vägen på riktigt (se ovan). | `winget install OpenJS.NodeJS.LTS`, sedan `npm install -g @openai/codex` |
| **Claude Code** (valfritt) | Fortsätta utvecklingen med Claude i terminalen. `CLAUDE.md` i repot ger Claude all bakgrund. | se [code.claude.com/docs](https://code.claude.com/docs) |
| **Inno Setup 6** (valfritt) | Bara för att bygga installationsprogrammet lokalt; GitHub Actions gör det annars åt dig. | `winget install JRSoftware.InnoSetup` |

Öppna en ny terminal efter installationerna, så att PATH uppdateras.

### Bygga och köra

```powershell
git clone https://github.com/Lonestarrk/ai-video-upscaler
cd ai-video-upscaler
cargo test --workspace                          # alla tester
cargo run -p upscaler-gui --release -- video.mp4  # bygg och starta, öppna en video direkt
```

- `--release` gör appen mycket snabbare (bildskalning och PNG-kodning är långsamma utan optimering). Exe-filen hamnar i `target\release\AIVideoUpscaler.exe`.
- Första bygget tar några minuter, eftersom alla beroenden kompileras. Sedan går det snabbt.

### Testvideor

```powershell
powershell -ExecutionPolicy Bypass -File scripts\make-test-videos.ps1
```

**Varför:** riktiga DVD-filer har egenheter som vanliga MP4-filer saknar: anamorfa pixlar, interlacing, svarta kanter och ibland fel flagga. Skriptet hämtar Sintel-trailern från Blender (fri licens, filmen är 2.35:1) och gör filer som täcker alla fallen. De hamnar i `test-videos\`, som git ignorerar.

| Fil | Vad den testar |
|---|---|
| `sintel_trailer-480p.mp4` | 16:9 med inbakad letterbox, som Blender publicerar den |
| `sintel_trailer-720p.mp4` | samma bild i högre upplösning, för att kontrollera mätningen |
| `sintel_dvd_ntsc.mpg` | NTSC-DVD: MPEG-2, anamorf 16:9, interlaced, AC3-ljud |
| `sintel_dvd_pal_43.vob` | PAL 4:3 med letterbox |
| `sintel_dvd_pal_43_full.vob` | PAL 4:3 med bild över hela ytan, bra för att testa "Ändra bildformat" till 16:9 |
| `sintel_wrong_flag_169.vob` | medvetet felflaggad, så att varningen ska visas |

Har du bara den FFmpeg som appen laddat ner, ange den: `-FFmpeg "$env:LOCALAPPDATA\AI Video Upscaler\tools\bin\ffmpeg.exe"`.

Bildrutenoggrannhet och bildformat kan kontrolleras mot de riktiga filerna:

```powershell
$env:UPSCALER_TEST_VIDEOS = (Get-ChildItem test-videos\* | % FullName) -join ";"
cargo test -p upscaler-core --test export frame_accuracy -- --ignored --nocapture
```

### Fortsätta med Claude Code

```powershell
cd ai-video-upscaler
claude
```

Claude läser `CLAUDE.md`, som beskriver arkitekturen, vad som är testat och en checklista över vad som ska testas mot de riktiga tjänsterna (först ChatGPT via Codex). Tips: tillåt vanliga kommandon (`cargo build/test/fmt`, `git status/diff/log`) med `/permissions`, så slipper du godkänna varje körning.

### Installationsprogram

GitHub Actions (`.github/workflows/build.yml`) bygger exe och installationsprogram med FFmpeg vid varje push. Lokalt:

```powershell
cargo build -p upscaler-gui --release
# lägg gärna ffmpeg.exe och ffprobe.exe i installer\ffmpeg\ så följer de med
& "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe" /DAppVersion=0.1.0 installer\upscaler.iss
```

## Felsökning

| Problem | Gör så här |
|---|---|
| Förhandsvisningen misslyckas | Felmeddelandet anger en mapp. Där ligger bilden som skickades och, för ChatGPT, `codex-output.txt` med allt Codex skrev. |
| "Codex is not signed in" | *Inställningar → ChatGPT → Logga in med ChatGPT*. Kontrollera i en terminal med `codex login status`. |
| Codex kan inte spara `output.png` | *Inställningar → ChatGPT → Avancerat → Sandlåda: danger-full-access*. Codex arbetar ändå bara i en tillfällig mapp per bildruta. |
| Planens gräns är nådd | Vänta och starta exporten igen. Den fortsätter där den slutade. |
| Förstärkaren är tyst eller spelar bara stereo | Välj spåret *5.1 (AC-3)* i spelaren. Alla förstärkare kan inte avkoda FLAC 5.1 som skickas vidare från TV:n eller spelaren. |
| Ljud-AI är långsamt | Kontrollera *Inställningar → Ljud-AI*: står det processorn, hittades inget grafikkort som stöds. Klassisk motor går snabbt utan AI. |
| Fönstret blir svart eller appen startar inte (gammalt grafikkort, virtuell maskin, fjärrskrivbord) | Starta med `set UPSCALER_RENDERER=glow` före `AIVideoUpscaler.exe`, så används OpenGL i stället. |
| Något annat | Loggen finns i `%LOCALAPPDATA%\AI Video Upscaler\upscaler.log` (*Inställningar → Allmänt → Öppna loggfilen*). |

## Hur det fungerar

### ChatGPT-prenumerationen

OpenAI:s officiella [Codex CLI](https://github.com/openai/codex) loggar in med ditt ChatGPT-konto, och dess inbyggda bildgenerering (gpt-image-2) räknas mot planens gränser i stället för att debiteras per anrop. För varje bildruta kör appen `codex exec` i en tillfällig mapp. Bildrutan och föregående resultat (som referens) bifogas, och Codex instrueras att redigera bilden med bildverktyget och spara den som `output.png`. Om filen hamnar någon annanstans hämtas den från `~/.codex/generated_images`. Appen tar bort `OPENAI_API_KEY` från miljön för Codex, så att anropen inte av misstag faktureras via API.

### Bildflödet

1. Bildrutan avkodas exakt (rätt bildruta, avflätad, utan svarta kanter, i kvadratiska pixlar).
2. Ska bildformatet ändras läggs den på en grå duk i det nya formatet, som AI:n ska fylla.
3. Om modellen bara svarar i vissa format läggs svarta kanter till.
4. Prompten byggs: din prompt plus appens tekniska regler.
5. AI:n svarar, kanterna beskärs bort och bilden skalas till exakt rätt storlek.
6. Vid export sätts bildrutorna ihop med originalljudet. Anamorfa källor kläms tillbaka till originalets pixelformat.

### Struktur och data

- `crates/upscaler-core`: allt utom gränssnittet: inställningar, FFmpeg, AI-tjänsterna (`provider/`), bildgeometri, bildformatkontroll, prompter, exportjobbet och ljudet (`audio/`: 5.1, LFE, kvalitetsförbättring och Ljud-AI).
- `crates/upscaler-gui`: egui-appen: jämförelsevy, tidslinje, sidopanel och inställningar.
- Inställningar: `%APPDATA%\AI Video Upscaler\settings.json`. Förhandsvisningar, exportjobb, nedladdad FFmpeg och logg: `%LOCALAPPDATA%\AI Video Upscaler\`. Arbetsmappen kan flyttas och tömmas i inställningarna; långa exporter kan ta många GB eftersom varje bildruta sparas som PNG.

## Licens

MIT, se [LICENSE](LICENSE). FFmpeg-bygget som följer med installationsprogrammet är GPL. Testvideon Sintel är © Blender Foundation, CC BY 3.0.
