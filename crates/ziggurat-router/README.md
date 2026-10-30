# ziggurat-router

Routeur Zigbee sur une radio OpenThread RCP (clé MG24) : il rejoint un réseau existant
une fois, relaie le trafic, répond aux interrogations du coordinateur, et échange des
trames avec les équipements par un port de contrôle local.

Par défaut il n'accepte aucun enfant : son beacon n'annonce aucune capacité. Avec
`--accept-children`, il prend des routeurs et des appareils endormis : il garde leurs
trames jusqu'à leur polling, répond pour eux aux requêtes d'adresse, et les retrouve
après un redémarrage.

## Enfants

L'appairage s'ouvre par le `Mgmt_Permit_Joining_req` du coordinateur, que le pont
`ziggurat-zigate` diffuse quand Abeille ouvre le réseau, ou par `permit <secondes>` sur
le port de contrôle. Pour chaque arrivée, le routeur envoie un Update-Device au trust center. Celui-ci
décide : s'il accepte, il renvoie la clé réseau dans un Tunnel que le routeur relaie à
l'enfant ; s'il refuse, l'enfant n'obtient rien. Le routeur n'a aucune clé de lien.

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
- `permit <secondes>`, en décimal, ouvre ce routeur aux enfants ; `0` le referme.
- `children` répond `children <ieee>:<nwk> ...`.
- `permit <secondes>`, en décimal, ouvre le routeur aux enfants ; `0` le referme.
- `children` répond `children <ieee>:<nwk> ...`.
- `status` répond `status <pan> <nwk> <canal> <pan étendu>`.
- `rx <source> <profile> <cluster> <src_ep> <dst_ep> <lqi> <rssi> <payload>` est émis
  pour chaque trame reçue ; le RSSI est en dBm décimal signé.

## Limites connues

Validé en simulation seulement, contre un coordinateur Ziggurat :

- enfants : rien de testé sur matériel ; Update-Device envoyé sans échange TCLK, avec le
  lien bien connu ; le rejoin d'un enfant et le départ n'ont pas de test de bout en bout ;
  les enfants sont sauvegardés toutes les 60 s, un enfant arrivé juste avant un arrêt
  brutal est perdu jusqu'à son prochain rejoin ;

- jamais testé sur une vraie radio ni face à une ZiGate ou un autre coordinateur ;
- l'adresse courte est programmée après la réponse d'association. Si le trust center
  envoie la clé tout de suite, la radio peut ne pas accuser réception à temps ;
- pas d'échange TCLK : le routeur garde le lien bien connu, un coordinateur qui impose
  une clé unique peut le retirer du réseau ;
- pas de rotation de clé réseau, ni de suivi d'un changement de canal ;
- émission de RouteRecord et relais de routage par source non vérifiés face à un vrai
  coordinateur ;
- le routeur lui-même ne sait pas quitter le réseau proprement : supprimer l'état et retirer l'appareil côté
  coordinateur ;
- pas de rejoin si le réseau change ;
- enfants : le rejoin et le départ sont écrits mais pas testés. Update-Device est chiffré
  avec le lien bien connu, un vrai trust center peut l'exiger autrement. Les compteurs
  de trames et l'horloge d'âge des enfants ne sont pas sauvegardés. Le serveur ZDO ne
  liste pas les enfants dans les réponses étendues.
