<?php
defined('BASEPATH') or exit('No direct script access allowed');

$route['default_controller'] = 'welcome';
$route['404_override'] = 'errors/page_missing';
$route['translate_uri_dashes'] = FALSE;
$route['shop'] = 'catalog/show/42';
$route['login']['post'] = 'welcome/login';
