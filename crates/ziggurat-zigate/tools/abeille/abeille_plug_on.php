<?php
require '/var/www/html/core/php/core.inc.php';
$eq = eqLogic::byLogicalId('Abeille1/1A01', 'Abeille');
$eq->getCmd('action', null, null, true);
foreach ($eq->getCmd('action') as $c) if ($c->getName() == 'On') { $c->execCmd(); echo "On envoyé\n"; }
sleep(6);
foreach ($eq->getCmd('info') as $c) if (in_array($c->getName(), ['Status','Active Power','RMS Current'])) echo "  ".$c->getName()." = ".json_encode($c->execCmd())." (".$c->getValueDate().")\n";
