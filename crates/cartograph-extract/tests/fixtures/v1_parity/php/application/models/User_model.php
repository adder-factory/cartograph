<?php
class User_model extends CI_Model
{
    public function find($id)
    {
        $this->load->model('audit_model');
        $this->audit_model->log($id);
        return $id;
    }
}
