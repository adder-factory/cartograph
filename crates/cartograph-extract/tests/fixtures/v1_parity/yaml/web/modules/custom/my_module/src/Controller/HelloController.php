<?php
namespace Drupal\my_module\Controller;

use Drupal\Core\Controller\ControllerBase;

class HelloController extends ControllerBase {
  public function build() {
    return ['#markup' => 'Hello'];
  }
}
