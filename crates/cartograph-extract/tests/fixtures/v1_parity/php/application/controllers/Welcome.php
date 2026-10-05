<?php
defined('BASEPATH') or exit('No direct script access allowed');

class Welcome extends CI_Controller
{
    public function index()
    {
        $this->load->model('user_model');
        $this->load->library('billing_lib', null, 'billing');
        $this->user_model->find(1);
        $this->billing->charge(5);
        $this->audit_model->log(1);
    }

    public function login()
    {
        return $this->helper();
    }

    private function helper()
    {
        return true;
    }
}
