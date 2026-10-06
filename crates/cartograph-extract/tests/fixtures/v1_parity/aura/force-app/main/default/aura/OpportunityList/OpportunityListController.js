({
    doInit: function(component, event, helper) {
        var action = component.get("c.fetch");
        action.setCallback(this, function(response) {
            component.set("v.rows", response.getReturnValue());
        });
        $A.enqueueAction(action);
        helper.loadAccounts(component);
    },
    handleSave: function(cmp, event, helper) {
        var save = cmp.get('c.save');
        $A.enqueueAction(save);
    },
    reload : function(component) {
        this.doInit(component);
    }
})
