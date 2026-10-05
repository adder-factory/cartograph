import { LightningElement, api } from 'lwc';

export default class OrderCard extends LightningElement {
  @api order;

  handleOpen() {
    this.dispatchEvent(new CustomEvent('open'));
  }
}
