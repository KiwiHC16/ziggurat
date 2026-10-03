<?php
require '/var/www/html/core/php/core.inc.php';
$eq = eqLogic::byLogicalId('Abeille2/9481', 'Abeille');
if (!$eq) { echo "pas d'équipement Abeille2/9481\n"; foreach (eqLogic::byType('Abeille') as $e) echo "  ".$e->getLogicalId()." ".$e->getName()."\n"; exit; }
echo "#{$eq->getId()} '{$eq->getName()}'\n";
echo "ab::eqModel = ".json_encode($eq->getConfiguration('ab::eqModel'))."\n";
echo "ab::zigbee = ".json_encode($eq->getConfiguration('ab::zigbee'))."\n";
foreach ($eq->getCmd() as $c) echo "  [".$c->getType()."] ".$c->getName()."\n";
