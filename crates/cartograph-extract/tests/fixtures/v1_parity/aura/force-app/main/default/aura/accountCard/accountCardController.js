({
    init: function(component) {
        var action = component.get("c.fetch");
        $A.enqueueAction(action);
    }
})
