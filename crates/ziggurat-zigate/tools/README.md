# Outils autour de la passerelle

Ce qui sert à faire tourner et observer l'environnement de test de la VM Claude
(Jeedom + Abeille + ziggurat-zigate + ziggurat-sim). Aucun secret ici : les mots de passe
restent hors du dépôt, la clé réseau est dans le fichier d'état (mode 0600).

## `jeedom-vm/install_jeedom_vm.sh`

Installation contrôlée de Jeedom (fork gitlab2 `jeedom/core` cloné dans
`~/dev/jeedomvm/core`) : Apache, PHP 8.4, MariaDB 11.8, sans `dist-upgrade` ni fail2ban.
`sudo` de `www-data` sans élévation (les `sudo <cmd>` d'Abeille s'exécutent en
`www-data`). Reprend une installation interrompue. À lancer en root :
`su -c 'sh install_jeedom_vm.sh'`.

Nécessite le correctif `DB::Prepare` (branche `ziggurat` de `jeedom/core`) sous PHP 8.

## Plugins Jeedom

Depuis le 03/10, la passerelle et le simulateur tournent sous Jeedom, par deux plugins
(dépôts gitlab2 `jeedom/zigguratzigate` et `jeedom/zigguratsim`) : un équipement par
passerelle ou par simulateur, démons surveillés par Jeedom, logs `zigguratzigate_<id>` et
`zigguratsim_<id>`, fichiers d'état dans `plugins/zigguratzigate/data/` (www-data, 0600,
interdits au web par `/etc/apache2/conf-available/ziggurat-data.conf`). Les programmes
sont dans `/usr/local/bin`. Les boutons des appareils simulés sont des commandes Jeedom.

## `flash/gecko_xmodem.py`

Envoi d'un firmware `.gbl` au bootloader Gecko d'une clé Silicon Labs (Sonoff Dongle Plus
MG24), en XMODEM-CRC, sans aucune dépendance (Python 3.7 suffit). Sert quand
`universal-silabs-flasher` ne peut pas tourner sur l'hôte où la clé est branchée (Python
trop ancien), et que le flash à travers un tunnel réseau (`socat`) échoue (menu du
bootloader désynchronisé).

1. Faire passer la clé dans son bootloader : `universal-silabs-flasher` depuis une autre
   machine, à travers un tunnel SSH + `socat` vers le port série (l'identification passe,
   avec `stty -crtscts -ixon -ixoff` sur le port avant `socat`), ou toute autre méthode.
2. Sur l'hôte de la clé : `python3 gecko_xmodem.py <tty> <fichier.gbl> --dry-run` (lit
   seulement le menu « Gecko Bootloader »), puis sans `--dry-run` pour envoyer le firmware
   (option 1 du menu, XMODEM-CRC par blocs de 128 octets) et lancer l'application (option 2).

Utilisé le 26/10/2026 pour passer la clé MG24 de jeedompro d'EmberZNet 7.4.5 à OpenThread
RCP 2.7.2.0 (960 blocs, « Serial upload complete »), vérifiée ensuite en Spinel à 460 800 bauds.

## `abeille/*.php`

Pilotage d'Abeille en ligne de commande, en `www-data` :
`runuser -u www-data -- php <script>` (depuis root).

| Script | Rôle |
|---|---|
| `abeille_inclusion.php` | Ouvre l'inclusion de la ZiGate 1 (`zgSetPermitMode`) |
| `abeille_group.php` | Met la prise 1A01 dans le groupe 7A01 et relit ses groupes |
| `abeille_plug.php` | Affiche l'équipement Abeille1/1A01 et ses commandes |
| `abeille_plug_cmd.php` | Off, On, Toggle sur la prise et relecture de l'état |
| `abeille_state.php` | État de la prise et appuis de la télécommande |
| `abeille_check.php` | IEEE retenue et informations de la ZiGate 1 |
| `abeille_reset_ieee.php` | Oublie l'IEEE de la ZiGate 1 et relance le démon |
| `abeille_setup.php` | Active Abeille et configure la ZiGate 1 en WIFI sur 127.0.0.1:20108 (simulateur) |
| `abeille_gtw2.php` | Configure la ZiGate 2 en WIFI sur 127.0.0.1:20109 (clé MG24 réelle) |
| `abeille_list_gtw2.php` | Liste les équipements de la ZiGate 2 et leur modèle |
| `abeille_eq_info.php` | Détail d'un équipement (modèle, infos Zigbee, commandes) — adresse à adapter |
| `abeille_plug_on.php` | Allume la prise simulée et relit l'état |
| `abeille_stdin.php` | Lit des lignes `sujet|paramètres` sur l'entrée standard et les passe à Abeille (`msgToCmd`), ex. `CmdAbeille2/8734/readAttribute|ep=01&clustId=0001&attrId=0020,0021`. Envoyer tout d'un bloc : Abeille enchaîne les requêtes, y compris vers un appareil sur pile pendant ses réveils |

## `wireshark/`

- `colorfilters-butineuse` : les couleurs du plugin Butineuse (`zb.lua`) en règles
  permanentes (plus la Route Request, `zbee_nwk.cmd.id == 0x01`, en jaune clair), à placer en tête de `~/.config/wireshark/colorfilters`. Format :
  `@nom@filtre@[fond][texte]`, composantes sur 16 bits.
- `Zigbee réel.command` : idem pour la sonde de la passerelle sur la clé MG24 réelle
  (port 17761, `--capture`).
- `Zigbee simulé.command` : lanceur macOS. Relaie la capture de `ziggurat-sim`
  (port 17760) dans un FIFO et ouvre Wireshark dessus (l'interface graphique ne démarre
  pas sur `-i TCP@…`).

Déchiffrement : `~/.config/wireshark/zigbee_pc_keys` avec la clé réseau (champ
`network_key` du fichier d'état) et `ZigBeeAlliance09`
(`5A6967426565416C6C69616E63653039`).

## `../docs/blocs.html`

Schéma des blocs (environnement simulé, puis montage réel avec la clé MG24). Servi par
l'Apache de la VM sur http://192.168.4.124/ziggurat/ : copie dans `/var/www/ziggurat-doc/index.html`,
`Alias /ziggurat` dans `/etc/apache2/conf-available/ziggurat-doc.conf` (`a2enconf ziggurat-doc`).
Après une modification : `install -m 644 docs/blocs.html /var/www/ziggurat-doc/index.html` (en root).
