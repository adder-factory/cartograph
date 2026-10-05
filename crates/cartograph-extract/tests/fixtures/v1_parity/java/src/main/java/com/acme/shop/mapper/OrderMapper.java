package com.acme.shop.mapper;

import com.acme.shop.model.Order;
import org.apache.ibatis.annotations.Mapper;
import org.apache.ibatis.annotations.Param;
import org.mybatis.spring.SqlSessionTemplate;

@Mapper
public interface OrderMapper {
    Order findOrder(@Param("orderId") String orderId, int unused);

    int deleteOrder(@Param("orderId") String orderId);
}

class OrderAttributeDaoImpl {
    private static final String SQL_NS = OrderAttributeDao.class.getName() + "Mapper";
    private SqlSessionTemplate sqlSessionTemplate;

    public int deleteByOrderId(String id) {
        return getSqlSessionTemplate().delete(SQL_NS + ".deleteByOrderId", id);
    }

    public Object findAttr(String id) {
        return sqlSessionTemplate.selectOne("com.acme.shop.mapper.OrderAttributeDaoMapper.findAttr", id);
    }

    private SqlSessionTemplate getSqlSessionTemplate() { return sqlSessionTemplate; }
}

interface OrderAttributeDao {}
