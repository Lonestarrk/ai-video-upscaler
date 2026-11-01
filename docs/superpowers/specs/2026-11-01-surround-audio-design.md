# Ljud: 5.1 från stereo och förbättrad ljudkvalitet

Datum: 2026-11-01. Underlag: ljudforskningsrapporten (`scratchpad/audio-research/report.md`,
sammanfattad här där det behövs) och webbresearch om ljudtjänster samma dag.

## Varför

Ägaren skalar upp gamla TV-serier (testfall: Star Trek Voyager S01E01, 1995) och tittar på
dem via **AV-receiver med riktiga 5.1-högtalare och subwoofer**. Bilden blir modern, men
ljudet är kvar som 90-talets stereo. Målet är att ljudet ska höja upplevelsen på samma sätt:
äkta 5.1 med dialog i mitten, rymd bakifrån och bas i subwoofern vid effekter, och om man
vill en renare, "högre bitrate"-känsla, utan att avsnittet slutar låta som sig självt.

## Vad källan innehåller (uppmätt på testfilen)

- AC-3 stereo, 48 kHz, 192 kbit/s, **Dolby Surround-flaggad** (dsurmod = 2): stereon bär en
  matrisad mitt (L+R) och en mono-bakkanal (L−R, cirka 100 Hz–7 kHz).
- Bas ner till cirka 35–40 Hz, nästan inget under 30 Hz. Starka basmoment finns i cirka 11 %
  av speltiden (till exempel 01:00:34, 00:21:38, 01:15:19).
- Diskanten tar slut vid cirka 14 kHz (TV-sändningens gräns, inte AC-3:ans).
- −26,8 LUFS, true peak −7,8 dBTP, dialnorm −27.

## Ägarens beslut

1. **Tre fristående val** i appen, i samma projekt som bilden:
   - **Ljudformat:** *Oförändrat* (originalet kopieras, som i dag) · *5.1 klassisk* (utan AI) ·
     *5.1 med AI* (lokal).
   - **Förbättra ljudkvaliteten:** av/på. Fungerar på stereo (behåll stereo, bara bättre) och
     på alla 5.1-kanaler utom LFE. Kan väljas utan 5.1 och 5.1 kan väljas utan den.
   - **Bas till subwoofern (LFE):** reglage med nivåerna *Subtil*, *Normal*, *Kraftig* och
     *Biokänsla*. Biokänsla lägger dessutom till syntetisk djupbas under effekterna.
2. **AI-separering från början** (dialog, musik, effekter), körd **lokalt**. **Förvalet** för
   ljudet är den lokala AI-modellen **på grafikkortet**: när 5.1 eller bättre kvalitet slås på
   är *med AI (lokal, grafikkort)* förvalt, inte klassisk. Klassisk 5.1 väljs aktivt.
   Processorn används bara som reserv när grafikkortet inte fungerar.
3. **Klassisk 5.1** byggs också: snabb, alltid fungerande, reserv och jämförelse.
4. **Molntjänster byggs inte nu**, men ljudet får ett **eget tjänsteval, oberoende av bildens
   tjänst**, så att MVSep (bäst uppmätt separering, billigt API) eller AudioShake kan läggas
   till senare utan omtag.
5. **Appen hämtar AI-paketet själv** (knapp i Inställningar, som "Hämta FFmpeg").
6. **Om grafikkortet inte kan användas körs AI:n på processorn** (2–4 h per avsnitt); appen
   säger det och visar uppskattad tid innan exporten startar.
7. **Provlyssning** sker genom att exportera ett kort urval (start/slut), precis som för bild.
8. **Utfil med 5.1:** MKV med tre ljudspår: FLAC 5.1 24-bit/48 kHz (förval), AC-3 5.1
   640 kbit/s, originalets ljud kopierat orört.
9. **Ljudet bearbetas i sektioner**, inte som en enda lång fil, och sektionerna fogas ihop till
   sist.
10. ChatGPT/Codex används inte för ljud (OpenAI har ingen sådan tjänst).
11. **Appen är privat** (bara ägaren). Alla modeller får användas, även de med oklar eller
    icke-kommersiell licens; de hämtas från originalkällorna. Ingen licenssida behövs nu.
12. **Klassisk 5.1 får en försiktig LFE:** grinden reagerar bara på riktig djupbas (30–60 Hz,
    jämfört med energin i röstläget 100–300 Hz) och har striktare tröskel. *Biokänsla* går bara
    att välja med AI. Kravet "dialog aldrig i subben" gäller fullt ut för AI-vägen; för den
    klassiska vägen gäller "så sällan som möjligt".

## Mål

- Dialogen hörs från mitten och blir inte sämre eller annorlunda.
- Musik och effekter breder ut sig fram och bak; ambiens och efterklang hamnar bakåt.
- Subwoofern används bara vid effekter (explosioner, träffar, motorer, warp, dörrar), aldrig av
  dialog, och den pumpar inte.
- Ljudet ligger exakt i synk med bilden, också vid ett kort urval.
- Allt som är gjort sparas i projektet; en avbruten körning fortsätter där den slutade.
- Att byta LFE-nivå eller ljudval gör bara om ljudet och sätter ihop det med den redan
  kodade bilden, utan nya AI-bilder.

## Inte mål (nu)

- Molntjänster för ljud (förberedda, inte byggda).
- DTS, TrueHD, Atmos, Dolby-logotyper eller Dolby-namn i gränssnittet (skriv "AC-3").
- Uppspelning av 5.1 inne i appen.
- 5.1-källor (till exempel DVD-utgåvor): ska upptäckas, och då är "5.1 från stereo" inte
  valbart; förbättringen får fortfarande användas.

## Får inte hända (även om det tekniskt "fungerar")

- Rösterna låter annorlunda, burkiga eller robotaktiga.
- Dialog i subwoofern eller dialog som läcker tydligt till bakkanalerna.
- Ekon eller kamfiltrering för att samma ljud ligger i flera kanaler med olika fördröjning.
- Hörbara skarvar mellan sektioner.
- Synkfel (en fast fördröjning på 43 ms från FFmpegs `surround`-filter är ett känt fall).
- Originalljudet försvinner ur filen.
- Påhittad "ny" diskant som väsljud eller metallisk glans.

## Hur det ska fungera

### Ljudjobbet

Ett eget **ljudjobb** per projekt och val, i `<projektmapp>/audio/<nyckel>/`, där nyckeln
bygger på videons identitet, exportens start/slut och alla ljudval som påverkar resultatet
(på samma sätt som bildjobben). Stegen:

1. **Läs in:** avkoda exportens tidsintervall av källans ljud en gång till 48 kHz (förlustfritt
   mellanformat).
2. **Kontrollera källan:** kanaler, Dolby Surround-flagga (AC-3-huvudet: efter syncword 0x0B77,
   bsid/bsmod/acmod; vid acmod = 2 är nästa 2 bitar dsurmod), dialnorm, ljudstyrka. Är källan
   redan 5.1 används inte 5.1-konverteringen.
3. **Dela i sektioner** med överlapp. Varje färdig sektion sparas; finns den redan hoppas den
   över.
4. **Per sektion**, beroende på valen:
   - *5.1 med AI:* separera i tal, musik och effekter (lokal modell), placera i 5.1 enligt
     nedan.
   - *5.1 klassisk:* FFmpegs `surround`-filter plus egen bakkanal och LFE enligt nedan.
   - *Förbättra kvaliteten:* per stam/kanal (före placering när AI används), aldrig på LFE.
5. **Foga ihop** sektionerna med mjuk övertoning i överlappen.
6. **Kvalitetskontroll** (se nedan) och sedan **mixa ihop** med bilden.

Exporten kodar bilden till en egen fil i bildjobbet. Det sista steget sätter ihop bild och ljud.
När bara ljudvalen ändras körs ljudjobbet och ihopsättningen om, inte bildrutorna.

### Placering i 5.1 (AI)

- **C:** talstammen i mono, högpass 60–80 Hz. Ingen musik.
- **L/R:** musik + främre delen av effekterna + **resten** (källan minus summan av stammarna,
  så att inget försvinner) + det som inte är mono i talet.
- **Ls/Rs:** bakdelen av musik och effekter (L−R-delen, högpass 100 Hz, lågpass cirka 7 kHz),
  fördröjd 10–20 ms och avkorrelerad mellan vänster och höger; eventuellt efterklang från en
  avklangsmodell, 6–9 dB lägre.
- **LFE:** bara från effektstammen, enligt nästa avsnitt.
- Samma signal får aldrig ligga i C och L/R med olika fördröjning.

### Placering i 5.1 (klassisk)

FFmpegs `surround` (chl_out 5.1, utan dess egen LFE), bakkanalerna omdöpta till SL/SR och
fördröjda, egen LFE från mono-summan. Filtrets fördröjning på 2048 sampel kompenseras i alla
parallella vägar. Ett fungerande kommando finns i rapporten (avsnitt 6, steg 3).

### LFE (subwoofern)

- Källa: effektstammen (AI) eller mono-summan (klassisk).
- **Grind** på 30–100 Hz-energin: öppnar bara vid basmoment (tröskel cirka −45 dBFS eller
  avsnittets 85:e percentil, attack 5 ms, release 200–300 ms). Dialog får aldrig öppna den.
- **Lågpass 90 Hz** (brant, linjär fas) och **högpass 20 Hz**. Aldrig över 120 Hz.
- Skrivs 10 dB under önskad hörd nivå (receivern lägger på +10 dB). Toppar högst −1 dBFS.
- Huvudkanalerna behåller **all** sin bas. LFE lägger bara till; receiverns bashantering tar
  hand om resten.
- Nivåer (i filen, relativt huvudkanalernas bas; hörd ökning vid basmoment inom parentes):
  - *Subtil:* −16 dB (cirka +3,5 dB)
  - *Normal:* −13 dB (cirka +4,6 dB), förval
  - *Kraftig:* −10 dB (cirka +6 dB)
  - *Biokänsla:* som Kraftig **plus syntetisk djupbas**: oktav-ner (dbx 120A-princip):
    hitta 50–90 Hz, skapa en ton en oktav under med samma kontur, lågpass 55 Hz, blanda 6–12 dB
    under, bara när grinden är öppen, bara effekter, hoppa över brusiga partier.

### Förbättrad ljudkvalitet

- **Bandskarvning:** originalet behålls orört under brytfrekvensen (hittas per fil där
  spektrumet faller kraftigt, cirka 13,5 kHz för testfilen). Bara bandet ovanför tas från
  modellen, via en linjärfas-delning, och dess nivå begränsas till en naturlig fortsättning av
  spektrumets lutning.
- Modeller: **Apollo** (lagar artefakter från förlustkomprimering) och **FlashSR** (fyller i
  diskant, snabb). Ingen av dem har oberoende lyssningstester; därför ska varje steg gå att
  slå av och jämföra.
- Stereo: hela stereon. 5.1 med AI: per stam före placering. 5.1 klassisk: L/R som par,
  Ls/Rs som par, C för sig. Aldrig LFE.

### Lokal AI

- Ett **AI-ljudpaket** hämtas av appen till `%LOCALAPPDATA%\AI Video Upscaler\tools\audio-ai`:
  fristående Python, PyTorch, MSST (ZFTurbo Music-Source-Separation-Training) och modellerna.
- **Grafikkort:** AMD:s PyTorch för Windows (ROCm) listar RX 7900 XTX. Fungerar den inte körs
  allt på processorn. Appen visar vilket som används och uppskattad tid.
- Separeringsmodell: BandIt Plus (DnR, MIT-kod) eller Bandit-v2 (vikter CC BY-SA 4.0); välj
  efter licens och kvalitet.

### Gränssnitt

- Sidopanelen får avsnittet **Ljud** med de tre valen och en rad om vad som kommer att hända
  (tid, var det körs, utformat).
- Inställningar får fliken **Ljud-AI**: hämta/ta bort paketet, visa grafikkort eller
  processor, test-körning.
- Exportpanelen visar ljudjobbets steg och sektioner, som för bild.
- Valen sparas i projektet (`project.json`), inte globalt.

### Utfil

| Ljudval | Behållare | Ljudspår |
|---|---|---|
| Oförändrat | MP4 (som i dag) | originalet |
| Bara bättre kvalitet (stereo) | MKV | FLAC stereo (förval) + originalet |
| 5.1 (med eller utan bättre kvalitet) | MKV | FLAC 5.1 24-bit (förval) + AC-3 5.1 640 kbit/s + originalet |

Kanalordning normaliseras till `5.1(side)`. AC-3 får `dialnorm −27` (eller källans värde).
Ingen PCM i MP4 eller MKV (fungerar dåligt).

### Kvalitetskontroll

- **Nedmixningstest:** 5.1 mixat tillbaka till stereo minus originalet ska ge ≤ cirka −35 dBFS
  för klassisk väg (uppmätt −38,5 dB när fördröjningen är rätt, −22 dB när den inte är det;
  fångar synkfel).
- Ljudstyrka inom ±1 LU från källan, true peak ≤ −1 dBTP.
- Kanalordningstest med en testfil med sex olika toner.

## Begränsningar

- Windows i första hand, AMD RX 7900 XTX, Rust + egui, FFmpeg finns redan.
- Långa avsnitt (testfilen 92 minuter): mellanfiler över 4 GB kräver RF64/W64 eller FLAC.
- Lokal AI på processorn tar timmar; användaren ska veta det innan.

## Beslut tagna av Claude (kontrollera gärna)

- **Kvalitetskontroll stoppar inte exporten.** Ljudstyrka och toppar rättas automatiskt
  (förstärkning till källans LUFS ±1 LU, begränsare till −1 dBTP). Nedmixningstestet har en
  gräns bara för klassisk väg (≤ −35 dBFS) och fungerar som synkkontroll; för AI-vägen och vid
  förbättrad kvalitet kontrolleras synken i stället med korskorrelation mot källan (fördröjning
  ska vara 0 sampel ±1) och ljudstyrkan. Underkänt ger en varning i exportpanelen och loggen,
  och ljudjobbet kan göras om.
- **Första versionen: Windows, AMD-grafikkort (ROCm) eller processorn.** CUDA och macOS byggs
  inte nu men paketet utformas så att fler varianter kan läggas till.
- **Stereo utan Dolby Surround-flagga** (eller AAC/MP3-källor) får också 5.1: bakkanalerna
  byggs av det som skiljer vänster och höger (samma metod), utan antagandet att det finns en
  matrisad bakkanal. **Mono-källor** får inte 5.1 från stereo (valet är grått med förklaring);
  förbättringen fungerar.
- **5.1-källa + bättre kvalitet:** utfilen blir FLAC 5.1 + AC-3 5.1 + originalet. Källans LFE
  lämnas orörd och subwoofer-reglaget är grått.
- **Dolby Surround-stereo som bara förbättras** bearbetas som mitt/sida (M/S), så att
  förhållandet mellan vänster och höger består och en receiver fortfarande kan avkoda den
  dolda mitten och bakkanalen. Bandskarvningen ändrar dessutom bara det som ligger över cirka
  13,5 kHz, där sidosignalen nästan saknas.

## Lämnat åt byggaren

- Sektionslängd, överlapp och övertoning.
- Exakt hur Python-miljön hämtas, uppdateras och isoleras, och hur appen pratar med den.
- Filterdesign (FIR), avkorrelering, gränsvärden i grinden inom ramarna ovan.
- Val mellan BandIt Plus och Bandit-v2, och om Apollo, FlashSR eller båda används.
- Hur ljudtjänsterna abstraheras så att molntjänster kan läggas till senare.

## Testa först (på riktiga filen)

Teasern, 01:00:25 (basmoment), en dialogscen på bryggan och signaturmelodin: lyssna på
receivern, kör nedmixningstestet, och prova AMD-grafikkortet med separeringen.
