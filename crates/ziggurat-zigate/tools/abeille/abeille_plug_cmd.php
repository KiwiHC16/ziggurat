<?php
require '/var/www/html/core/php/core.inc.php';
$eq = eqLogic::byLogicalId('Abeille1/1A01', 'Abeille');
$show = function() use ($eq) {
  foreach ($eq->getCmd('info') as $c) if (in_array($c->getName(), ['Status','Active Power','RMS Current'])) echo "    ".$c->getName()." = ".json_encode($c->execCmd())."\n";
};
foreach (['Off', 'On', 'Toggle'] as $name) {
  foreach ($eq->getCmd('action') as $c) if ($c->getName() == $name) $c->execCmd();
  sleep(5);
  echo "$name :\n"; $show();
}
