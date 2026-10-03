<?php
require '/var/www/html/core/php/core.inc.php';
include_once '/var/www/html/plugins/Abeille/core/config/Abeille.config.php';
$r = eqLogic::byLogicalId('Abeille1/1A02', 'Abeille');
if ($r) {
  $m = $r->getConfiguration('ab::eqModel', []);
  echo "Télécommande #{$r->getId()} '{$r->getName()}' modèle=".($m['modelName'] ?? $m['id'] ?? '?')." type=".($m['type'] ?? '')."\n";
  foreach ($r->getCmd() as $c) echo "  [".$c->getType()."] ".$c->getName()."\n";
} else echo "pas encore d'équipement pour 1A02\n";
Abeille::msgToCmd(PRIO_NORM, 'CmdAbeille1/1A01/addGroup', 'ep=01&group=7A01');
sleep(3);
Abeille::msgToCmd(PRIO_NORM, 'CmdAbeille1/1A01/getGroupMembership', 'ep=01');
sleep(4);
$p = eqLogic::byLogicalId('Abeille1/1A01', 'Abeille');
foreach ($p->getCmd('info') as $c) if (stripos($c->getLogicalId(), 'group') !== false || stripos($c->getName(), 'group') !== false) echo "Prise ".$c->getName()." = ".json_encode($c->execCmd())."\n";
echo "Prise ab::zigbee groups = ".json_encode($p->getConfiguration('ab::zigbee', [])['groups'] ?? null)."\n";
