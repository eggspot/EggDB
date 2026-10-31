<?php

declare(strict_types=1);

require __DIR__ . '/../src/EggDbException.php';
require __DIR__ . '/../src/Connection.php';

function check(bool $condition, string $what): void
{
    if (!$condition) {
        fwrite(STDERR, "php driver check failed: $what\n");
        exit(1);
    }
}

$db = new EggDB\Connection(':memory:');
$db->execute('CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, score REAL)');
$db->execute('INSERT INTO people (name, score) VALUES (?, ?)', ['Ada', 1.5]);
$db->execute('INSERT INTO people (name, score) VALUES (?, ?)', [null, 2]);
$rows = $db->query('SELECT id, name, score FROM people ORDER BY id');
check($rows[0] === ['id' => 1, 'name' => 'Ada', 'score' => 1.5], 'the first row');
check($rows[1]['name'] === null, 'a NULL name');
try {
    $db->query('SELECT * FROM nowhere');
    check(false, 'a missing table fails');
} catch (EggDB\EggDbException $error) {
    check(strlen($error->sqlState) === 5, 'the error has a SQLSTATE');
}
$db->close();
echo "php driver check: passed\n";
