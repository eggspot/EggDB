<?php

declare(strict_types=1);

namespace EggDB\Doctrine;

use Doctrine\DBAL\Driver\AbstractException;
use EggDB\EggDbException;

final class Exception extends AbstractException
{
    public static function from(EggDbException $error): self
    {
        $state = $error->sqlState !== '' ? $error->sqlState : null;
        return new self($error->getMessage(), $state, $error->getCode(), $error);
    }
}
