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
