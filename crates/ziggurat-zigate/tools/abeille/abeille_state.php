<?php
require '/var/www/html/core/php/core.inc.php';
$p = eqLogic::byLogicalId('Abeille1/1A01', 'Abeille');
$r = eqLogic::byLogicalId('Abeille1/1A02', 'Abeille');
$v = function($eq, $name) { foreach ($eq->getCmd('info') as $c) if ($c->getName() == $name) return json_encode($c->execCmd())." @".substr($c->getValueDate(), 11); return '?'; };
echo "  prise Status=".$v($p,'Status')."  Puissance=".$v($p,'Active Power')."  | télécommande On=".$v($r,'On')." Off=".$v($r,'Off')."\n";
