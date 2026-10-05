<?php
namespace App\Support;

const APP_VERSION = '2.0';

class Config
{
    public string $env = 'local';
}

class Opts
{
}

function helper_fn(Config $cfg, ?Opts $o = null): string
{
    return $cfg->env;
}

function format_name(string $name): string
{
    return strtoupper($name);
}
