<?php
header('Content-Type: text/plain');
$pdo = new PDO('mysql:host=db;dbname=db', 'db', 'db');
echo 'asagiri-ddev:' . $pdo->query('SELECT 21 * 2')->fetchColumn() . "\n";
