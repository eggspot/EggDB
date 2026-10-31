<?php

declare(strict_types=1);

namespace EggDB;

final class EggDbException extends \RuntimeException
{
    public function __construct(string $message, public readonly string $sqlState, int $status)
    {
        parent::__construct($message, $status);
    }
}
