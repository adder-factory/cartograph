({
  doInit: function (component, event, helper) {
    var action = component.get('c.loadOrders');
    helper.enqueue(component, action);
  },
  handleClick: function (component) {
    component.set('v.open', true);
  },
});
