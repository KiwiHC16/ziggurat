# ziggurat-router

Routeur Zigbee sans enfants, sur une radio OpenThread RCP (clé MG24) : il rejoint un
réseau existant une fois, relaie le trafic, répond aux interrogations du coordinateur,
et échange des trames avec les équipements par un port de contrôle local.

Il n'accepte aucun enfant : son beacon n'annonce aucune capacité.

## Lancement

    ziggurat-router --device /dev/ttyUSB0 --flow-control software \
        --state router.json --channels 20 --extended-pan-id <16 chiffres hex>

Ouvrir l'appairage sur le coordinateur avant le premier lancement. Sans
`--extended-pan-id`, le réseau ouvert le plus fort est rejoint : à éviter en dehors d'un
réseau de test, le voisin est peut-être ouvert. Si `router.json` existe, il n'y a pas de
nouveau join. Ce fichier contient la clé réseau (mode 0600) et il faut le sauvegarder :
sans lui, il faut rejoindre à nouveau.

## Port de contrôle

`127.0.0.1:20109`, sans authentification : à garder sur la boucle locale. Une commande ou
un événement par ligne, valeurs en hexadécimal :

- `send <nwk> <profile> <cluster> <src_ep> <dst_ep> <payload>` répond `ok` ou `err <raison>`.
  Un Toggle ZCL vers `1234`, endpoint 1 : `send 1234 0104 0006 01 01 012a02`.
- `status` répond `status <pan> <nwk> <canal> <pan étendu>`.
- `rx <source> <profile> <cluster> <src_ep> <dst_ep> <lqi> <rssi> <payload>` est émis
  pour chaque trame reçue ; le RSSI est en dBm décimal signé.

## Limites connues

Validé en simulation seulement, contre un coordinateur Ziggurat :

- jamais testé sur une vraie radio ni face à une ZiGate ou un autre coordinateur ;
- l'adresse courte est programmée après la réponse d'association. Si le trust center
  envoie la clé tout de suite, la radio peut ne pas accuser réception à temps ;
- pas d'échange TCLK : le routeur garde le lien bien connu, un coordinateur qui impose
  une clé unique peut le retirer du réseau ;
- pas de rotation de clé réseau, ni de suivi d'un changement de canal ;
- émission de RouteRecord et relais de routage par source non vérifiés face à un vrai
  coordinateur ;
- pas de quitter le réseau proprement : supprimer l'état et retirer l'appareil côté
  coordinateur ;
- pas de rejoin si le réseau change.
