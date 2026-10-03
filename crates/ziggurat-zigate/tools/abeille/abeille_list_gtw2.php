<?php
require '/var/www/html/core/php/core.inc.php';
foreach (eqLogic::byType('Abeille') as $e) if (strpos($e->getLogicalId(), 'Abeille2/') === 0) {
  $m = $e->getConfiguration('ab::eqModel', []); $z = $e->getConfiguration('ab::zigbee', []);
  echo $e->getLogicalId()." #".$e->getId()." '".$e->getName()."' modèle=".($m['modelName'] ?? '-')." rxOn=".json_encode($z['rxOnWhenIdle'] ?? null)."\n";
}
