<?php

declare(strict_types=1);

require __DIR__ . '/../src/EggDbException.php';
require __DIR__ . '/../src/Connection.php';
require __DIR__ . '/../src/Statement.php';
require __DIR__ . '/../src/KeyValue.php';
require __DIR__ . '/../src/Transaction.php';

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

$insert = $db->prepare('INSERT INTO people (name, score) VALUES (?, ?)');
check($insert->parameterCount === 2, 'two parameters');
$insert->execute(['Grace', 3.25]);
$insert->execute(['Edsger', 4]);
$insert->finalize();
$names = [];
foreach ($db->iterate('SELECT name FROM people WHERE score > ? ORDER BY id', [3]) as $row) {
    $names[] = $row['name'];
}
check($names === ['Grace', 'Edsger'], 'stepped rows');
$pg = new EggDB\Connection(':memory:', 'postgresql');
$pg->execute('CREATE TABLE t (id integer, name text)');
check($pg->copyFrom('COPY t FROM STDIN', ["1\tad", "a\n2\tgrace\n"]) === 2, 'COPY loads two rows');
check($pg->copyTo('COPY t TO STDOUT') === "1\tada\n2\tgrace\n", 'COPY reads the rows back');
$pg->close();
$db->close();

$path = sys_get_temp_dir() . '/eggdb-php-kv-' . getmypid() . '.eggdb';
$store = new EggDB\KeyValue($path);
$store->write(function (EggDB\Transaction $txn): void {
    $txn->put('a', '1');
    $txn->put('b', "2\0two");
    check($txn->createCollection('users'), 'a collection is created');
    $txn->put('u1', 'Ada', 'users');
});
$store->read(function (EggDB\Transaction $txn): void {
    check($txn->get('a') === '1', 'a stored value');
    check($txn->get('missing') === null, 'a missing key');
    check(iterator_to_array($txn->scan()) === ['a' => '1', 'b' => "2\0two"], 'a scan with binary values');
    check($txn->collections() === ['users'], 'the collection names');
    check($txn->collectionStats('users')['entries'] === 1, 'the collection stats');
});
$store->write(function (EggDB\Transaction $txn): void {
    check($txn->delete('a'), 'a key is deleted');
    $txn->renameCollection('users', 'people');
});
$store->read(function (EggDB\Transaction $txn): void {
    check($txn->get('a') === null, 'the deleted key is gone');
    check($txn->get('u1', 'people') === 'Ada', 'the renamed collection keeps its keys');
});
$store->close();
@unlink($path);
echo "php driver check: passed\n";
