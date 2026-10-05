package native

/*
#include <stdio.h>
static void do_thing(int n) { (void)n; }
*/
import "C"

type Client struct{}

func (c *Client) do_thing() {}

func RunIt() {
	C.do_thing(42)
	C.puts(nil)
}

func NotCgo(c *Client) {
	c.do_thing()
}
