<?php
namespace Drupal\mymod\Plugin\Block;

/**
 * Provides a hello block.
 *
 * @Block(
 *   id = "hello_block",
 *   admin_label = @Translation("Hello")
 * )
 */
class HelloBlock
{
    public function build(): array
    {
        return ['#markup' => 'hello'];
    }
}

#[Block(id: "goodbye_block")]
class GoodbyeBlock
{
    public function build(): array
    {
        return [];
    }
}
