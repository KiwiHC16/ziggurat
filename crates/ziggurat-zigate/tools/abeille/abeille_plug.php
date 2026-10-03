<?php
require '/var/www/html/core/php/core.inc.php';
$eq = eqLogic::byLogicalId('Abeille1/1A01', 'Abeille');
if (!$eq) { echo "pas d'équipement Abeille1/1A01\n"; exit; }
echo "Equipement #{$eq->getId()} '{$eq->getName()}' actif=".$eq->getIsEnable()."\n";
$m = $eq->getConfiguration('ab::eqModel', []);
echo "Modèle: ".json_encode(['id'=>$m['modelName'] ?? $m['id'] ?? '', 'manuf'=>$m['manufacturer'] ?? '', 'model'=>$m['model'] ?? '', 'type'=>$m['type'] ?? '']).", statut=".json_encode($eq->getConfiguration('ab::zigbee', [])['status'] ?? '')."\n";
foreach ($eq->getCmd() as $c) echo "  [".$c->getType()."] ".$c->getName()." = ".($c->getType()=='info' ? json_encode($c->execCmd()) : '')."\n";
