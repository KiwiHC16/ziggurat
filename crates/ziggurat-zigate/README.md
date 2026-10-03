# ziggurat-zigate : passerelle Abeille ↔ Ziggurat

Émulateur de **ZiGate v1** (protocole série ZiGate sur TCP) au-dessus de la pile
**Ziggurat**. Le plugin Jeedom **Abeille** s'y connecte comme à une ZiGate WiFi ;
derrière, la radio est une clé OpenThread RCP (Sonoff Dongle Plus MG24).

Objectif : garder Abeille, ses modèles, ses scénarios et JeedomConnect, et remplacer la
pile NXP fermée des ZiGate (14 NPDU, fuite de tampons, redémarrages « Ndpu stuck ») par
une pile ouverte, sans limite de tampons, qu'on peut lire et corriger.

```
Jeedom / Abeille ──socat──TCP 20108──▶ ziggurat-zigate ──Spinel──▶ clé MG24 (OpenThread RCP)
                  (ZiGate v1, mode RAW)  ├─ émulation ZiGate          (ou ziggurat-sim)
                                         ├─ ZDO du coordinateur
                                         ├─ persistance (≈ PDM)
                                         └─ ZigbeeStack (en process)
```

État au 03/10/2026 (branche locale `abeille-bridge`, base `dev` 392d4c7) :

| Brique | État |
|---|---|
| Codec de trames ZiGate (`frame.rs`) | fait, testé |
| Messages et réponses ZiGate (`msg.rs`) | builders 8000/8002/8010/8011/8014/8024/004D/8048 faits |
| Mode `observe` (sans radio) | fait : décode et journalise ce qu'envoie Abeille, répond à la poignée de main |
| Simulateur de clé (`crates/ziggurat-sim`) | fait, testé avec la pile complète |
| Mode `run` (passerelle complète) | à faire |

## Lancer

```bash
# Observer Abeille sans radio (pointer une ZiGate WiFi d'Abeille sur <IP VM>:20108)
cargo run -p ziggurat-zigate -- observe

# Plus tard : passerelle complète
cargo run -p ziggurat-zigate -- run --device /dev/ttyUSB0 --baudrate 460800
cargo run -p ziggurat-zigate -- run --device tcp://127.0.0.1:9000   # sur le simulateur
```

Côté Abeille (ZiGate n° X) : `ab::gtwType = zigate`, `ab::gtwSubType = WIFI`,
`ab::gtwIpAddr = <ip>:20108` (sans `:port`, Abeille utilise 9999). Si on remplace une
ZiGate existante, vider `ab::zgIeeeAddrX` ou réutiliser son IEEE (voir plus bas).

## Contrat côté Abeille

Source : Abeille `260705-BETA-2`, commit `ad3ac1ded` (`~/dev/prod/jeedompro/git/plugins/Abeille`).

### Transport et trames
- TCP brut : `socat pty,raw,echo=0,link=/tmp/zigateWifiX tcp:<ip>:<port>`, aucune négociation.
- Abeille → ZiGate : `01 | esc(type:2 len:2 crc:1 payload) | 03`, `len` = charge seule.
- ZiGate → Abeille : idem **plus un octet LQI** avant `03`, compté dans `len` et dans le CRC.
- CRC = XOR de type, longueur, charge (et LQI). Échappement : tout octet < 0x10 → `02, b^0x10`.
- Champs natifs en big-endian ; ZDO/ZCL bruts (0530/8002) en little-endian.

### Démarrage (`configureZigate()`), une 0x8000 attendue pour chacune
`0021` masque de canaux → `0016` heure → `0002 01` mode RAW → `0019` certification →
`0023 00` coordinateur → `0024` démarrage → `0010` version (à t+10 s).

Réponses indispensables :
- **0x8024** après 0024 : `status addr(2) ieee(8) canal`. Abeille épingle l'IEEE du
  coordinateur ; s'il change, il ignore tout sauf 0x8000. **IEEE stable obligatoire.**
- **0x8010** : `major(2) minor(2)`. Annoncer `0004-0323`. Sans 8010, les annonces 004D
  sont ignorées.
- **0x8010 toutes les minutes** (le cron envoie 0010) : sans réponse depuis 2 min, Abeille
  relance socat et coupe le TCP.
- **0x8014** en réponse à 0014 (état d'inclusion), relancé chaque minute pendant l'inclusion.
- 0x8007 = redémarrage « usine » : déclenche une reconfiguration complète.

### Contrôle de flux (`AbeilleCmdQueue.class.php`)
- 3 commandes en vol au plus, 50 ms entre deux écritures, **une seule commande en vol par appareil**.
- **0x8000 appariée strictement en FIFO** sur la commande la plus ancienne, par `packetType` :
  exactement une 0x8000 par commande reçue, dans l'ordre, `packetType` = type de la commande.
- Format 8 octets : `status sqn packetType(2) sent sqnAps nPdu aPdu`.
- Commandes avec accusé (addrMode 02/03) : 0x8000 puis **0x8011 avec le même `sqnAps`**,
  sinon délai de 8 s puis abandon sans renvoi (4 s pour les autres).
- **NPDU** : si > 7, Abeille ne passe plus que les commandes « gateway-only » ; s'il reste
  non nul et figé 4 min, il envoie 0011 (soft reset). La passerelle renvoie **NPDU=APDU=0**.

### Commandes à traiter (mode RAW, production)
- **0530** trame APS brute : `addrMode dst(2) srcEp dstEp cluster(2) profile(2) secMode radius len data`
  (attention : cluster **avant** profile). C'est l'essentiel du trafic ZCL et une partie du ZDO.
- Natives encore utilisées, à **traduire en trames ZDO/ZCL** par la passerelle :
  0100 (lecture d'attributs, la plus utilisée), 0110, 0045, 0043, 0042, 0041, 0040, 0030, 0031,
  004E, 0047, 004C, 0026, 0060/0062/0063/0064, 0070, 0092/0093, 0081/0084, 00B7, 00C0, 00FA.
- Gateway-only : 0002, 0009, 0010, 0011, 0012, 0014, 0015, 0016, 0017, 0018, 0019, 0021, 0023, 0024, 0049, 0806, 0807.
- Optionnel : OTA 0500/0505/0502 (Abeille sert lui-même les blocs via 8002), scènes 00A0-00A6, touchlink 00D2, AB0x (firmware Abeille).

### Messages à produire
- **8002** pour **toutes** les réponses ZDO et ZCL (y compris celles des commandes natives) :
  `status profile(2) cluster(2) srcEp dstEp srcMode src(2) dstMode dst(2) asdu`.
- **004D** annonce d'appareil : `addr(2) ieee(8) macCapa`. Déclenche l'identification
  (0045 → 0043 → 0042 → 0100 cluster 0000 → modèle → bind + configureReporting).
- **8048** départ : `ieee(8) rejoin`. 8012/8702 optionnels (journal seulement).
- La ZiGate répond seule à NWK_addr_req, IEEE_addr_req, Match_Desc ; Mgmt_Lqi sur 0000 doit
  revenir en 8002 cluster 8031.

## Contrat côté Ziggurat

Source : `crates/ziggurat-driver`, `crates/ziggurat-server`, `crates/ziggurat-protocol` (dev 392d4c7).

- On embarque `SpinelPhy` + `ZigbeeStack` dans le processus (comme `ziggurat-server`),
  sans passer par le WebSocket.
- **La pile ne garde rien** : la passerelle sauvegarde et restaure clé réseau, PAN, canal,
  compteurs de trames NWK et APS (notifications tous les 100), clés de lien uniques
  (`LinkKeyUpdate`), enfants, table d'adresses IEEE↔NWK, routes source. C'est notre « PDM ».
- **ZDO du coordinateur à notre charge** : la pile ne répond qu'à Mgmt_Lqi et Mgmt_Rtg (et les
  transmet quand même). Node_Desc, Active_EP, Simple_Desc, IEEE/NWK_addr, Match_Desc… sont à
  écrire dans la passerelle.
- Ouvrir l'inclusion = `permit_joins()` **plus** un Mgmt_Permit_Joining_req diffusé à 0xFFFC.
- `DeviceJoined` arrive après la remise de la clé, Device_annce (cluster 0013) ensuite :
  dédoublonner avant d'émettre 004D.
- Pas de renvoi APS ni de fragmentation : un envoi avec accusé = un essai, 5 s (10 s si
  appareil sur pile). Renvois éventuels côté passerelle.
- Envois par adresse NWK uniquement (résoudre l'IEEE via la table d'adresses).
- File de notifications : 64 entrées, la plus ancienne est jetée. La vider sans attendre.
- Correspondance des erreurs : `handed_off` → 8000 (admission) / 8702 ; `delivered` → 8011
  (Ok → 00, `ApsAckTimeout` → A7) ; NwkNoAck → E9, CcaFailure → E1, routes → D0, IndirectExpired → F0.
- Le lecteur Spinel fait `process::exit(1)` si la liaison série tombe : lancer sous systemd.

## Plan

1. **Observer** (sans clé) : `observe` sur la VM, une ZiGate WiFi de test dans Abeille
   (jeedomdev, ou un Jeedom de test sur la VM) pointée sur `<IP VM>:20108`. Valider la
   poignée de main et relever la séquence réelle.
2. **Cœur `run`** sur `ziggurat-sim` : ouverture radio, formation du réseau, persistance JSON
   (écriture atomique), 0530 → `send_aps_*`, réception → 8002, 0x8000/0x8011 avec `sqnAps`,
   inclusion 0049/0014, 004D/8048.
3. **ZDO coordinateur** + traduction des commandes natives (0100, 0045, 0043, 0042, 0041,
   0030/0031, 004E, 0060/0062, 0092, 0081…).
4. **Appareils simulés** dans `ziggurat-sim` (routeur avec réponses ZDO/ZCL de base : on/off,
   cluster 0000) pour tester l'identification complète par Abeille sans matériel.
5. **Clé réelle** : flash OpenThread RCP, passage USB à la VM, réseau de test sur le **canal 25**
   avec 2-3 prises de rechange, en parallèle de la production.
6. **Migration** ZiGate par ZiGate, en commençant par JnJ, avec un routeur vers le cellier.

## Mise en service de la clé Sonoff Dongle Plus MG24

1. Flasher le firmware **OpenThread RCP** avec l'outil web Sonoff
   (<https://dongle.sonoff.tech/sonoff-dongle-flasher/>). Noter la version et le débit.
2. Ce firmware n'a **pas de contrôle de flux matériel** : lancer avec `--flow-control none`
   (option à ajouter au mode `run`) et le débit du firmware (115200 ou 460800).
3. Le firmware Ziggurat du dépôt (`firmwares/efr32mg24`) vise la carte BRD2601B (broches,
   quartz) : **ne pas l'utiliser** sur la clé Sonoff. La pile tourne sur l'hôte, un RCP standard suffit.
4. VM Claude : QEMU arm64. Il faut passer la clé USB depuis l'hôte de la VM, et donner
   l'accès série à `ben` (`usermod -aG dialout ben` en root, ou règle udev).

## Tests sans matériel

`crates/ziggurat-sim` simule la clé :
- `Medium` : l'air 802.15.4 (filtrage d'adresses, accusés MAC, renvois, liaisons réglables
  par sens : perte, RSSI, LQI) ;
- `SimPhy` : une radio en process pour faire tourner des piles dans les tests ;
- `rcp::serve` / binaire `ziggurat-sim` : une fausse clé OpenThread RCP en Spinel sur TCP.

```bash
cargo test -p ziggurat-sim -p ziggurat-zigate
cargo run -p ziggurat-sim -- --listen 127.0.0.1:9000
```

Les tests de bout en bout couvrent : démarrage de la pile sur la fausse clé Spinel, envoi APS
avec accusé entre un coordinateur et un routeur simulés (en direct et via Spinel), et une
liaison à sens unique (le cas du cellier) qui perd l'accusé.
