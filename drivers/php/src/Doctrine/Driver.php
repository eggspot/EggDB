<?php

declare(strict_types=1);

namespace EggDB\Doctrine;

use Doctrine\DBAL\Driver\AbstractSQLiteDriver;
use EggDB\EggDbException;

final class Driver extends AbstractSQLiteDriver
{
    public function connect(array $params): Connection
    {
        $path = empty($params['memory']) ? (string) ($params['path'] ?? ':memory:') : ':memory:';
        try {
            return new Connection(new \EggDB\Connection($path, 'sqlite'));
        } catch (EggDbException $error) {
            throw Exception::from($error);
        }
    }
}
